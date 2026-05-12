import pathlib

import pytest

from gremlins.executor.gremlin import Gremlin
from gremlins.pipeline import Pipeline as _PipelineData
from gremlins.pipeline.discovery import resolve_pipeline_name, resolve_pipeline_path
from gremlins.pipeline.loader import STAGE_TYPES
from gremlins.stages.base import Stage
from gremlins.stages.github_wait_copilot import GitHubWaitCopilot
from gremlins.stages.loop import LoopStage
from gremlins.stages.parallel import ParallelStage
from gremlins.stages.plan import Plan


def _pipeline_data(stages: list[Stage] | None = None) -> _PipelineData:
    return _PipelineData(name="test", path=pathlib.Path("."), stages=stages or [])


def _local(
    stages: list[Stage],
    *,
    resume_from: str | None = None,
    tmp_path: pathlib.Path,
) -> Gremlin:
    return Gremlin(
        stages,
        state_dir=tmp_path,
        session_dir=tmp_path,
        gr_id=None,
        pipeline_data=_pipeline_data(stages),
        resume_from=resume_from,
    )


def test_pipeline_constructs_from_local_yaml(tmp_path: pathlib.Path) -> None:
    pipeline_data = _PipelineData.from_yaml(
        resolve_pipeline_path("local", pathlib.Path.cwd())
    )
    gremlin = Gremlin(
        pipeline_data.stages,
        state_dir=tmp_path,
        session_dir=tmp_path,
        gr_id=None,
        pipeline_data=pipeline_data,
    )

    assert len(gremlin.stages) > 0
    assert all(isinstance(s, Stage) for s in gremlin.stages)
    stage_types = [s.type for s in gremlin.stages]
    assert "plan" in stage_types
    assert "implement" in stage_types
    assert "plan" in STAGE_TYPES
    assert "implement" in STAGE_TYPES
    assert "review-code" in STAGE_TYPES
    assert "address-code" in STAGE_TYPES
    assert "verify" in STAGE_TYPES


def test_pipeline_constructs_from_gh_yaml(tmp_path: pathlib.Path) -> None:
    pipeline_data = _PipelineData.from_yaml(
        resolve_pipeline_path("gh", pathlib.Path.cwd())
    )
    gremlin = Gremlin(
        pipeline_data.stages,
        state_dir=tmp_path,
        session_dir=tmp_path,
        gr_id=None,
        pipeline_data=pipeline_data,
        repo="",
        state_file=None,
    )

    assert len(gremlin.stages) > 0
    assert all(isinstance(s, Stage) for s in gremlin.stages)
    stage_types = [s.type for s in gremlin.stages]
    assert "plan" in stage_types
    assert "implement" in stage_types
    assert "plan" in STAGE_TYPES
    assert "implement" in STAGE_TYPES
    assert "github-open-pull-request" in STAGE_TYPES
    assert "github-request-copilot-review" in STAGE_TYPES
    assert "github-review-pull-request" in STAGE_TYPES
    assert "github-address-pull-request-reviews" in STAGE_TYPES
    assert "github-wait-ci" in STAGE_TYPES
    assert "github-wait-copilot" in STAGE_TYPES


# ---------------------------------------------------------------------------
# validate_resume_target tests
# ---------------------------------------------------------------------------


def _make_stages(*names: str) -> list[Stage]:
    return [Plan(n, None, [], {}) for n in names]


def _make_parallel_stage(name: str, children: list[str]) -> ParallelStage:
    child_stages: list[Stage] = [Plan(c, None, [], {}) for c in children]
    return ParallelStage(name, child_stages)


def test_validate_resume_target_no_resume_from(tmp_path: pathlib.Path) -> None:
    gremlin = _local(
        _make_stages("plan", "implement"),
        resume_from=None,
        tmp_path=tmp_path,
    )
    gremlin.validate_resume_target()  # should not raise


def test_validate_resume_target_valid_name(tmp_path: pathlib.Path) -> None:
    gremlin = _local(
        _make_stages("plan", "implement"),
        resume_from="implement",
        tmp_path=tmp_path,
    )
    gremlin.validate_resume_target()  # should not raise


def test_validate_resume_target_invalid_name(tmp_path: pathlib.Path) -> None:
    gremlin = _local(
        _make_stages("plan", "implement"),
        resume_from="bogus",
        tmp_path=tmp_path,
    )
    with pytest.raises(ValueError, match="bogus"):
        gremlin.validate_resume_target()


def test_validate_resume_target_parallel_group_name(tmp_path: pathlib.Path) -> None:
    gremlin = _local(
        [_make_parallel_stage("reviews", ["review-a", "review-b"])],
        resume_from="reviews",
        tmp_path=tmp_path,
    )
    gremlin.validate_resume_target()  # "reviews" is a valid expanded name


def test_validate_resume_target_parallel_fanout_rejected(
    tmp_path: pathlib.Path,
) -> None:
    gremlin = _local(
        [_make_parallel_stage("reviews", ["review-a", "review-b"])],
        resume_from="reviews-fanout",
        tmp_path=tmp_path,
    )
    with pytest.raises(ValueError, match="reviews-fanout"):
        gremlin.validate_resume_target()  # fanout is internal, not a resume target


def test_validate_resume_target_parallel_fanin_rejected(tmp_path: pathlib.Path) -> None:
    gremlin = _local(
        [_make_parallel_stage("reviews", ["review-a", "review-b"])],
        resume_from="reviews-fanin",
        tmp_path=tmp_path,
    )
    with pytest.raises(ValueError, match="reviews-fanin"):
        gremlin.validate_resume_target()  # fanin is internal, not a resume target


def test_validate_resume_target_child_name_rejected(tmp_path: pathlib.Path) -> None:
    gremlin = _local(
        [_make_parallel_stage("reviews", ["review-a", "review-b"])],
        resume_from="review-a",
        tmp_path=tmp_path,
    )
    with pytest.raises(ValueError, match="review-a"):
        gremlin.validate_resume_target()


def test_pipeline_rejects_unknown_stage_type(tmp_path: pathlib.Path) -> None:
    s = Plan("s", None, [], {})
    s.type = "nonexistent"
    with pytest.raises(ValueError, match="nonexistent"):
        _local([s], tmp_path=tmp_path)


# ---------------------------------------------------------------------------
# GREMLINS_OVERLAY_DIR env-var override
# ---------------------------------------------------------------------------

_SAMPLE_YAML = """\
stages:
  - name: plan
    type: plan
"""


def test_pipeline_name_from_stem(tmp_path: pathlib.Path) -> None:
    yaml_path = tmp_path / "mypipe.yaml"
    yaml_path.write_text(_SAMPLE_YAML, encoding="utf-8")
    pipeline = _PipelineData.from_yaml(yaml_path)
    assert pipeline.name == "mypipe"


def test_pipeline_name_ignores_yaml_name_field(tmp_path: pathlib.Path) -> None:
    yaml_path = tmp_path / "mypipe.yaml"
    yaml_path.write_text("name: something-else\n" + _SAMPLE_YAML, encoding="utf-8")
    pipeline = _PipelineData.from_yaml(yaml_path)
    assert pipeline.name == "mypipe"


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
    gremlin = _local([plan_entry, parallel], tmp_path=tmp_path)

    stage_names = [s.name for s in gremlin.stages]
    assert "reviews" in stage_names
    assert "review-a" not in stage_names
    by_name = {s.name: s for s in gremlin.stages}
    assert by_name["reviews"].type == "parallel"


# ---------------------------------------------------------------------------
# Gremlin.needs_gh tests (via pipeline_data)
# ---------------------------------------------------------------------------


def _gh_stage(name: str = "gh-wait") -> GitHubWaitCopilot:
    return GitHubWaitCopilot(name, None, [], {})


def _local_stage(name: str = "plan") -> Plan:
    return Plan(name, None, [], {})


def _pipeline(*stages: Stage) -> _PipelineData:
    return _PipelineData(name="test", path=pathlib.Path("."), stages=list(stages))


def test_needs_gh_false_for_local_only_pipeline() -> None:
    assert not _pipeline(_local_stage("plan"), _local_stage("implement")).needs_gh()


def test_needs_gh_true_for_top_level_gh_stage() -> None:
    assert _pipeline(_local_stage("plan"), _gh_stage()).needs_gh()


def test_needs_gh_true_for_gh_stage_in_loop_body() -> None:
    loop = LoopStage(
        "boss-loop", body=[_local_stage("plan"), _gh_stage()], max_iterations=3
    )
    assert _pipeline(loop).needs_gh()


def test_needs_gh_false_for_local_stage_in_loop_body() -> None:
    loop = LoopStage(
        "boss-loop",
        body=[_local_stage("plan"), _local_stage("implement")],
        max_iterations=3,
    )
    assert not _pipeline(loop).needs_gh()


def test_stage_builders_registry_covers_all_known_types() -> None:
    expected = {
        "plan",
        "implement",
        "verify",
        "github-open-pull-request",
        "github-request-copilot-review",
        "github-review-pull-request",
        "github-wait-copilot",
        "github-address-pull-request-reviews",
        "github-wait-ci",
        "review-code",
        "address-code",
        "handoff",
        "parallel",
    }
    assert expected <= set(STAGE_TYPES)


def test_run_raises_without_initialize_runtime(tmp_path: pathlib.Path) -> None:
    gremlin = _local(_make_stages("plan"), tmp_path=tmp_path)
    with pytest.raises(RuntimeError, match="initialize_runtime"):
        gremlin.run()
