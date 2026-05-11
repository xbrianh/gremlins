import argparse
import pathlib

import pytest

from gremlins.orchestrators.pipeline import Pipeline
from gremlins.pipeline import Pipeline as _PipelineData
from gremlins.pipeline.discovery import resolve_pipeline_name, resolve_pipeline_path
from gremlins.stages.base import Stage
from gremlins.stages.parallel import ParallelStage
from gremlins.stages.plan import Plan
from gremlins.stages.registry import STAGE_REGISTRY


def _args(**kwargs: object) -> argparse.Namespace:
    return argparse.Namespace(**kwargs)


def _pipeline_data(stages: list[Stage] | None = None) -> _PipelineData:
    return _PipelineData(name="test", path=pathlib.Path("."), stages=stages or [])


def _local(
    stages: list[Stage],
    *,
    args: argparse.Namespace,
    tmp_path: pathlib.Path,
) -> Pipeline:
    return Pipeline(
        stages,
        args=args,
        session_dir=tmp_path,
        gr_id=None,
        pipeline_data=_pipeline_data(stages),
    )


def test_pipeline_constructs_from_local_yaml(tmp_path: pathlib.Path) -> None:
    pipeline_data = _PipelineData.from_yaml(
        resolve_pipeline_path("local", pathlib.Path.cwd())
    )
    pipe = Pipeline(
        pipeline_data.stages,
        args=_args(),
        session_dir=tmp_path,
        gr_id=None,
        pipeline_data=pipeline_data,
    )

    assert len(pipe.stages) > 0
    assert all(isinstance(s, Stage) for s in pipe.stages)
    stage_types = [s.type for s in pipe.stages]
    assert "plan" in stage_types
    assert "implement" in stage_types
    assert "plan" in STAGE_REGISTRY
    assert "implement" in STAGE_REGISTRY
    assert "review-code" in STAGE_REGISTRY
    assert "address-code" in STAGE_REGISTRY
    assert "verify" in STAGE_REGISTRY


def test_pipeline_constructs_from_gh_yaml(tmp_path: pathlib.Path) -> None:
    pipeline_data = _PipelineData.from_yaml(
        resolve_pipeline_path("gh", pathlib.Path.cwd())
    )
    pipe = Pipeline(
        pipeline_data.stages,
        args=_args(),
        session_dir=tmp_path,
        gr_id=None,
        pipeline_data=pipeline_data,
        repo="",
        state_file=None,
    )

    assert len(pipe.stages) > 0
    assert all(isinstance(s, Stage) for s in pipe.stages)
    stage_types = [s.type for s in pipe.stages]
    assert "plan" in stage_types
    assert "implement" in stage_types
    assert "plan" in STAGE_REGISTRY
    assert "implement" in STAGE_REGISTRY
    assert "open-github-pr" in STAGE_REGISTRY
    assert "request-copilot" in STAGE_REGISTRY
    assert "ghreview" in STAGE_REGISTRY
    assert "ghaddress" in STAGE_REGISTRY
    assert "wait-ci" in STAGE_REGISTRY
    assert "wait-copilot" in STAGE_REGISTRY


# ---------------------------------------------------------------------------
# validate_resume_target tests
# ---------------------------------------------------------------------------


def _make_stages(*names: str) -> list[Stage]:
    return [Plan(n, None, [], {}) for n in names]


def _make_parallel_stage(name: str, children: list[str]) -> ParallelStage:
    child_stages: list[Stage] = [Plan(c, None, [], {}) for c in children]
    return ParallelStage(name, child_stages)


def test_validate_resume_target_no_resume_from(tmp_path: pathlib.Path) -> None:
    pipe = _local(
        _make_stages("plan", "implement"),
        args=_args(resume_from=None),
        tmp_path=tmp_path,
    )
    pipe.validate_resume_target()  # should not raise


def test_validate_resume_target_valid_name(tmp_path: pathlib.Path) -> None:
    pipe = _local(
        _make_stages("plan", "implement"),
        args=_args(resume_from="implement"),
        tmp_path=tmp_path,
    )
    pipe.validate_resume_target()  # should not raise


def test_validate_resume_target_invalid_name(tmp_path: pathlib.Path) -> None:
    pipe = _local(
        _make_stages("plan", "implement"),
        args=_args(resume_from="bogus"),
        tmp_path=tmp_path,
    )
    with pytest.raises(ValueError, match="bogus"):
        pipe.validate_resume_target()


def test_validate_resume_target_parallel_group_name(tmp_path: pathlib.Path) -> None:
    pipe = _local(
        [_make_parallel_stage("reviews", ["review-a", "review-b"])],
        args=_args(resume_from="reviews"),
        tmp_path=tmp_path,
    )
    pipe.validate_resume_target()  # "reviews" is a valid expanded name


def test_validate_resume_target_parallel_fanout_rejected(
    tmp_path: pathlib.Path,
) -> None:
    pipe = _local(
        [_make_parallel_stage("reviews", ["review-a", "review-b"])],
        args=_args(resume_from="reviews-fanout"),
        tmp_path=tmp_path,
    )
    with pytest.raises(ValueError, match="reviews-fanout"):
        pipe.validate_resume_target()  # fanout is internal, not a resume target


def test_validate_resume_target_parallel_fanin_rejected(tmp_path: pathlib.Path) -> None:
    pipe = _local(
        [_make_parallel_stage("reviews", ["review-a", "review-b"])],
        args=_args(resume_from="reviews-fanin"),
        tmp_path=tmp_path,
    )
    with pytest.raises(ValueError, match="reviews-fanin"):
        pipe.validate_resume_target()  # fanin is internal, not a resume target


def test_validate_resume_target_child_name_rejected(tmp_path: pathlib.Path) -> None:
    pipe = _local(
        [_make_parallel_stage("reviews", ["review-a", "review-b"])],
        args=_args(resume_from="review-a"),
        tmp_path=tmp_path,
    )
    with pytest.raises(ValueError, match="review-a"):
        pipe.validate_resume_target()


def test_pipeline_rejects_unknown_stage_type(tmp_path: pathlib.Path) -> None:
    s = Plan("s", None, [], {})
    s.type = "nonexistent"
    with pytest.raises(ValueError, match="nonexistent"):
        _local([s], args=_args(), tmp_path=tmp_path)


# ---------------------------------------------------------------------------
# GREMLINS_OVERLAY_DIR env-var override
# ---------------------------------------------------------------------------

_SAMPLE_YAML = """\
name: sample
stages:
  - name: plan
    type: plan
"""


def _make_overlay(tmp_path: pathlib.Path, name: str) -> pathlib.Path:
    overlay = tmp_path / "overlay"
    overlay.mkdir(parents=True)
    (overlay / f"{name}.yaml").write_text(_SAMPLE_YAML, encoding="utf-8")
    return overlay


def test_resolve_pipeline_name_uses_overlay_dir(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    overlay = _make_overlay(tmp_path, "mylocal")
    monkeypatch.setenv("GREMLINS_OVERLAY_DIR", str(overlay))
    result = resolve_pipeline_name("mylocal", tmp_path / "project")
    assert result == (overlay / "mylocal.yaml").resolve()


def test_resolve_pipeline_path_uses_overlay_dir(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    overlay = _make_overlay(tmp_path, "mylocal")
    monkeypatch.setenv("GREMLINS_OVERLAY_DIR", str(overlay))
    result = resolve_pipeline_path("mylocal", tmp_path / "project")
    assert result == (overlay / "mylocal.yaml").resolve()


def test_resolve_pipeline_name_finds_project_dir_when_overlay_empty(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    empty_overlay = tmp_path / "empty_overlay"
    empty_overlay.mkdir()
    monkeypatch.setenv("GREMLINS_OVERLAY_DIR", str(empty_overlay))
    project_root = tmp_path / "project"
    pipeline_dir = project_root / ".gremlins"
    pipeline_dir.mkdir(parents=True)
    (pipeline_dir / "mylocal.yaml").write_text(_SAMPLE_YAML, encoding="utf-8")
    result = resolve_pipeline_name("mylocal", project_root)
    assert result == (pipeline_dir / "mylocal.yaml").resolve()


def test_resolve_pipeline_path_finds_project_dir_when_overlay_empty(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    empty_overlay = tmp_path / "empty_overlay"
    empty_overlay.mkdir()
    monkeypatch.setenv("GREMLINS_OVERLAY_DIR", str(empty_overlay))
    project_root = tmp_path / "project"
    pipeline_dir = project_root / ".gremlins"
    pipeline_dir.mkdir(parents=True)
    (pipeline_dir / "mylocal.yaml").write_text(_SAMPLE_YAML, encoding="utf-8")
    result = resolve_pipeline_path("mylocal", project_root)
    assert result == (pipeline_dir / "mylocal.yaml").resolve()


def test_resolve_pipeline_name_no_overlay_env_falls_through(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.delenv("GREMLINS_OVERLAY_DIR", raising=False)
    result = resolve_pipeline_name("local", pathlib.Path.cwd())
    assert result.name == "local.yaml"


def test_resolve_pipeline_path_no_overlay_env_falls_through(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.delenv("GREMLINS_OVERLAY_DIR", raising=False)
    result = resolve_pipeline_path("local", pathlib.Path.cwd())
    assert result.name == "local.yaml"


def test_parallel_expansion_in_constructor(tmp_path: pathlib.Path) -> None:
    parallel = _make_parallel_stage("reviews", ["review-a", "review-b"])
    plan_entry = Plan("plan", None, [], {})
    pipe = _local([plan_entry, parallel], args=_args(), tmp_path=tmp_path)

    stage_names = [s.name for s in pipe.stages]
    assert "reviews" in stage_names
    assert "review-a" not in stage_names
    by_name = {s.name: s for s in pipe.stages}
    assert by_name["reviews"].type == "parallel"


def test_stage_builders_registry_covers_all_known_types() -> None:
    expected = {
        "plan",
        "implement",
        "verify",
        "open-github-pr",
        "request-copilot",
        "ghreview",
        "wait-copilot",
        "ghaddress",
        "wait-ci",
        "review-code",
        "address-code",
        "handoff",
        "parallel",
    }
    assert expected <= set(STAGE_REGISTRY)
