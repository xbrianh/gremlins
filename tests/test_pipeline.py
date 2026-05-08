import argparse
import pathlib

import pytest

from gremlins.orchestrators.pipeline import Pipeline
from gremlins.pipeline import Pipeline as _PipelineData
from gremlins.pipeline import (
    StageEntry,
    load_pipeline,
    resolve_pipeline_name,
    resolve_pipeline_path,
)
from gremlins.stages import (
    address_code,
    commit,
    implement,
    open_github_pr,
    plan,
    request_copilot,
    review_code,
    verify,
    wait_ci,
    wait_copilot,
)


def _args(**kwargs: object) -> argparse.Namespace:
    return argparse.Namespace(**kwargs)


def _pipeline_data(stages: list[StageEntry] | None = None) -> _PipelineData:
    return _PipelineData(name="test", path=pathlib.Path("."), stages=stages or [])


def _local(
    stages: list[StageEntry],
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
        target="local",
    )


def test_pipeline_constructs_from_local_yaml(tmp_path: pathlib.Path) -> None:
    pipeline_data = load_pipeline(resolve_pipeline_path("local", pathlib.Path.cwd()))
    pipe = Pipeline(
        pipeline_data.stages,
        args=_args(),
        session_dir=tmp_path,
        gr_id=None,
        pipeline_data=pipeline_data,
        target="local",
    )

    assert len(pipe.stages) > 0
    assert all(isinstance(s, StageEntry) for s in pipe.stages)
    stage_types = [s.type for s in pipe.stages]
    assert "plan" in stage_types
    assert "implement" in stage_types
    assert pipe.target == "local"
    assert Pipeline.STAGE_TYPES["plan"] is plan.Plan
    assert Pipeline.STAGE_TYPES["implement"] is implement.Implement
    assert Pipeline.STAGE_TYPES["review-code"] is review_code.ReviewCode
    assert Pipeline.STAGE_TYPES["address-code"] is address_code.AddressCode
    assert Pipeline.STAGE_TYPES["verify"] is verify.Verify


def test_pipeline_constructs_from_gh_yaml(tmp_path: pathlib.Path) -> None:
    pipeline_data = load_pipeline(resolve_pipeline_path("gh", pathlib.Path.cwd()))
    pipe = Pipeline(
        pipeline_data.stages,
        args=_args(),
        session_dir=tmp_path,
        gr_id=None,
        pipeline_data=pipeline_data,
        repo="",
        target="github",
        state_file=None,
    )

    assert len(pipe.stages) > 0
    assert all(isinstance(s, StageEntry) for s in pipe.stages)
    stage_types = [s.type for s in pipe.stages]
    assert "plan" in stage_types
    assert "implement" in stage_types
    assert pipe.target == "github"
    assert Pipeline.STAGE_TYPES["plan"] is plan.Plan
    assert Pipeline.STAGE_TYPES["implement"] is implement.Implement
    assert Pipeline.STAGE_TYPES["commit"] is commit.Commit
    assert Pipeline.STAGE_TYPES["open-github-pr"] is open_github_pr.OpenGitHubPR
    assert Pipeline.STAGE_TYPES["request-copilot"] is request_copilot.RequestCopilot
    assert Pipeline.STAGE_TYPES["ghreview"] is review_code.ReviewCode
    assert Pipeline.STAGE_TYPES["ghaddress"] is address_code.AddressCode
    assert Pipeline.STAGE_TYPES["wait-ci"] is wait_ci.WaitCI
    assert Pipeline.STAGE_TYPES["wait-copilot"] is wait_copilot.WaitCopilot


# ---------------------------------------------------------------------------
# validate_resume_target tests
# ---------------------------------------------------------------------------


def _make_stages(*names: str) -> list[StageEntry]:
    return [
        StageEntry(name=n, type="plan", client=None, prompt_paths=[], options={})
        for n in names
    ]


def _make_parallel_stage(name: str, children: list[str]) -> StageEntry:
    child_entries = [
        StageEntry(name=c, type="plan", client=None, prompt_paths=[], options={})
        for c in children
    ]
    return StageEntry(
        name=name,
        type="parallel",
        client=None,
        prompt_paths=[],
        options={},
        children=child_entries,
    )


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


def test_validate_resume_target_parallel_fanout(tmp_path: pathlib.Path) -> None:
    pipe = _local(
        [_make_parallel_stage("reviews", ["review-a", "review-b"])],
        args=_args(resume_from="reviews-fanout"),
        tmp_path=tmp_path,
    )
    pipe.validate_resume_target()  # fanout is valid


def test_validate_resume_target_parallel_fanin(tmp_path: pathlib.Path) -> None:
    pipe = _local(
        [_make_parallel_stage("reviews", ["review-a", "review-b"])],
        args=_args(resume_from="reviews-fanin"),
        tmp_path=tmp_path,
    )
    pipe.validate_resume_target()  # fanin is valid


def test_validate_resume_target_child_name_rejected(tmp_path: pathlib.Path) -> None:
    pipe = _local(
        [_make_parallel_stage("reviews", ["review-a", "review-b"])],
        args=_args(resume_from="review-a"),
        tmp_path=tmp_path,
    )
    with pytest.raises(ValueError, match="review-a"):
        pipe.validate_resume_target()


def test_pipeline_rejects_unknown_stage_type(tmp_path: pathlib.Path) -> None:
    stages = [
        StageEntry(name="s", type="nonexistent", client=None, prompt_paths=[], options={})
    ]
    with pytest.raises(ValueError, match="nonexistent"):
        _local(stages, args=_args(), tmp_path=tmp_path)


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
    (overlay / "pipelines").mkdir(parents=True)
    (overlay / "pipelines" / f"{name}.yaml").write_text(_SAMPLE_YAML, encoding="utf-8")
    return overlay


def test_resolve_pipeline_name_uses_overlay_dir(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    overlay = _make_overlay(tmp_path, "mylocal")
    monkeypatch.setenv("GREMLINS_OVERLAY_DIR", str(overlay))
    result = resolve_pipeline_name("mylocal", tmp_path / "project")
    assert result == (overlay / "pipelines" / "mylocal.yaml").resolve()


def test_resolve_pipeline_path_uses_overlay_dir(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    overlay = _make_overlay(tmp_path, "mylocal")
    monkeypatch.setenv("GREMLINS_OVERLAY_DIR", str(overlay))
    result = resolve_pipeline_path("mylocal", tmp_path / "project")
    assert result == (overlay / "pipelines" / "mylocal.yaml").resolve()


def test_resolve_pipeline_name_finds_project_dir_when_overlay_empty(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    empty_overlay = tmp_path / "empty_overlay"
    empty_overlay.mkdir()
    monkeypatch.setenv("GREMLINS_OVERLAY_DIR", str(empty_overlay))
    project_root = tmp_path / "project"
    pipeline_dir = project_root / ".gremlins" / "pipelines"
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
    pipeline_dir = project_root / ".gremlins" / "pipelines"
    pipeline_dir.mkdir(parents=True)
    (pipeline_dir / "mylocal.yaml").write_text(_SAMPLE_YAML, encoding="utf-8")
    result = resolve_pipeline_path("mylocal", project_root)
    assert result == (pipeline_dir / "mylocal.yaml").resolve()


def test_resolve_pipeline_name_no_overlay_env_falls_through(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.delenv("GREMLINS_OVERLAY_DIR", raising=False)
    result = resolve_pipeline_name("local", pathlib.Path.cwd())
    assert result.name == "local.yaml"


def test_resolve_pipeline_path_no_overlay_env_falls_through(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.delenv("GREMLINS_OVERLAY_DIR", raising=False)
    result = resolve_pipeline_path("local", pathlib.Path.cwd())
    assert result.name == "local.yaml"


def test_parallel_expansion_in_constructor(tmp_path: pathlib.Path) -> None:
    parallel = _make_parallel_stage("reviews", ["review-a", "review-b"])
    plan_entry = StageEntry(
        name="plan", type="plan", client=None, prompt_paths=[], options={}
    )
    pipe = _local([plan_entry, parallel], args=_args(), tmp_path=tmp_path)

    stage_names = [s.name for s in pipe.stages]
    assert all(s.type != "parallel" for s in pipe.stages)
    assert "reviews-fanout" in stage_names
    assert "reviews" in stage_names
    assert "reviews-fanin" in stage_names
    assert "review-a" not in stage_names
    by_name = {s.name: s for s in pipe.stages}
    assert by_name["reviews-fanout"].type == "parallel-fanout"
    assert by_name["reviews"].type == "parallel-group"
    assert by_name["reviews-fanin"].type == "parallel-fanin"
