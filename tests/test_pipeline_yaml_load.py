"""Load tests for bundled pipeline YAMLs."""

import pathlib
import textwrap

import pytest

from gremlins.clients.client import Client
from gremlins.pipeline import Pipeline
from gremlins.pipeline.loader import _fill_names  # type: ignore[reportPrivateUsage]

_BUNDLED_LOCAL = (
    pathlib.Path(__file__).parent.parent / "gremlins" / "pipelines" / "local.yaml"
)

_LOCAL_STAGE_NAMES = ["plan", "implement", "review-code", "address-code", "verify"]


def test_bundled_local_loads() -> None:
    pipeline = Pipeline.from_yaml(_BUNDLED_LOCAL)
    assert pipeline.default_client == Client("claude", "sonnet")
    assert [s.name for s in pipeline.stages] == _LOCAL_STAGE_NAMES
    for stage in pipeline.stages:
        assert stage.client == Client("claude", "sonnet")


def test_bad_default_client_rejected(tmp_path: pathlib.Path) -> None:
    bad = tmp_path / "pipeline.yaml"
    bad.write_text(
        textwrap.dedent("""\
            default_client: bogus:foo
            stages:
              - { name: plan, type: plan }
        """),
        encoding="utf-8",
    )
    with pytest.raises(ValueError, match="unknown provider"):
        Pipeline.from_yaml(bad)


# --- _fill_names unit tests ---


def _stages(*types: str) -> list[dict]:
    return [{"type": t} for t in types]


def test_fill_names_defaults_to_type() -> None:
    raw = _stages("plan", "implement", "verify")
    _fill_names(raw)
    assert [d["name"] for d in raw] == ["plan", "implement", "verify"]


def test_fill_names_collision_gets_suffix() -> None:
    raw = _stages("verify", "verify", "verify")
    _fill_names(raw)
    assert [d["name"] for d in raw] == ["verify", "verify-2", "verify-3"]


def test_fill_names_explicit_wins() -> None:
    raw = [{"name": "my-plan", "type": "plan"}, {"type": "implement"}]
    _fill_names(raw)
    assert raw[0]["name"] == "my-plan"
    assert raw[1]["name"] == "implement"


def test_fill_names_explicit_name_reserves_slot() -> None:
    # explicit {name: verify} then unnamed {type: verify} → second gets verify-2
    raw = [{"name": "verify", "type": "implement"}, {"type": "verify"}]
    _fill_names(raw)
    assert raw[0]["name"] == "verify"
    assert raw[1]["name"] == "verify-2"


def test_fill_names_parallel_key_uses_parallel_type() -> None:
    raw = [{"parallel": [{"type": "verify"}]}, {"parallel": [{"type": "plan"}]}]
    _fill_names(raw)
    assert raw[0]["name"] == "parallel"
    assert raw[1]["name"] == "parallel-2"


def test_fill_names_mixed_explicit_and_default_no_collision() -> None:
    raw = [
        {"name": "verify", "type": "verify"},
        {"type": "verify"},
        {"type": "verify"},
    ]
    _fill_names(raw)
    assert raw[0]["name"] == "verify"
    assert raw[1]["name"] == "verify-2"
    assert raw[2]["name"] == "verify-3"


# --- integration: name-optional pipeline loading ---


def _write_pipeline(tmp_path: pathlib.Path, content: str) -> pathlib.Path:
    p = tmp_path / "pipeline.yaml"
    p.write_text(textwrap.dedent(content), encoding="utf-8")
    return p


def test_pipeline_name_optional_defaults_to_type(tmp_path: pathlib.Path) -> None:
    p = _write_pipeline(
        tmp_path,
        """\
        default_client: claude:sonnet
        stages:
          - { type: plan }
          - { type: implement }
          - { type: verify }
        """,
    )
    pipeline = Pipeline.from_yaml(p)
    assert [s.name for s in pipeline.stages] == ["plan", "implement", "verify"]


def test_pipeline_duplicate_unnamed_stages_auto_numbered(
    tmp_path: pathlib.Path,
) -> None:
    p = _write_pipeline(
        tmp_path,
        """\
        default_client: claude:sonnet
        stages:
          - { type: plan }
          - { type: verify }
          - { type: verify }
          - { type: verify }
        """,
    )
    pipeline = Pipeline.from_yaml(p)
    assert [s.name for s in pipeline.stages] == [
        "plan",
        "verify",
        "verify-2",
        "verify-3",
    ]


def test_pipeline_explicit_name_overrides_default(tmp_path: pathlib.Path) -> None:
    p = _write_pipeline(
        tmp_path,
        """\
        default_client: claude:sonnet
        stages:
          - { name: ci-gate, type: github-wait-ci }
          - { type: plan }
        """,
    )
    pipeline = Pipeline.from_yaml(p)
    assert pipeline.stages[0].name == "ci-gate"
    assert pipeline.stages[1].name == "plan"


def test_pipeline_nested_scopes_disambiguate_independently(
    tmp_path: pathlib.Path,
) -> None:
    p = _write_pipeline(
        tmp_path,
        """\
        default_client: claude:sonnet
        stages:
          - { type: verify }
          - name: checks
            parallel:
              - { type: verify }
              - { type: verify }
        """,
    )
    pipeline = Pipeline.from_yaml(p)
    assert pipeline.stages[0].name == "verify"
    parallel = pipeline.stages[1]
    assert parallel.name == "checks"
    assert [c.name for c in parallel.body] == ["verify", "verify-2"]
