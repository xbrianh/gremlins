import argparse
import pathlib

import pytest

from gremlins.orchestrators.pipeline import GHPipeline, LocalPipeline
from gremlins.pipeline import Pipeline as _PipelineData
from gremlins.pipeline import StageEntry, load_pipeline, resolve_pipeline_path
from gremlins.stages import (
    address_code,
    commit,
    ghaddress,
    ghplan,
    ghreview,
    implement,
    open_github_pr,
    plan,
    request_copilot,
    review_code,
    verify,
    wait_ci,
    wait_copilot,
)


def _args(**kwargs):
    return argparse.Namespace(**kwargs)


def _pipeline_data(stages: list[StageEntry] | None = None) -> _PipelineData:
    return _PipelineData(name="test", path=pathlib.Path("."), stages=stages or [])


def _local(stages, *, args, tmp_path):
    return LocalPipeline(
        stages,
        args=args,
        session_dir=tmp_path,
        gr_id=None,
        pipeline_data=_pipeline_data(stages),
    )


def test_local_pipeline_constructs_from_bundled_yaml(tmp_path):
    pipeline_data = load_pipeline(resolve_pipeline_path("local", pathlib.Path.cwd()))
    pipe = LocalPipeline(
        pipeline_data.stages,
        args=_args(),
        session_dir=tmp_path,
        gr_id=None,
        pipeline_data=pipeline_data,
    )

    assert len(pipe.stages) > 0
    assert all(isinstance(s, StageEntry) for s in pipe.stages)
    stage_types = [s.type for s in pipe.stages]
    assert "plan" in stage_types
    assert "implement" in stage_types
    assert pipe.target == "local"
    assert LocalPipeline.STAGE_TYPES["plan"] is plan.Plan
    assert LocalPipeline.STAGE_TYPES["implement"] is implement.Implement
    assert LocalPipeline.STAGE_TYPES["review-code"] is review_code.ReviewCode
    assert LocalPipeline.STAGE_TYPES["address-code"] is address_code.AddressCode
    assert LocalPipeline.STAGE_TYPES["verify"] is verify.Verify


def test_gh_pipeline_constructs_from_bundled_yaml(tmp_path):
    pipeline_data = load_pipeline(resolve_pipeline_path("gh", pathlib.Path.cwd()))
    pipe = GHPipeline(
        pipeline_data.stages,
        args=_args(),
        session_dir=tmp_path,
        gr_id=None,
        pipeline_data=pipeline_data,
        repo="",
        state_file=None,
    )

    assert len(pipe.stages) > 0
    assert all(isinstance(s, StageEntry) for s in pipe.stages)
    stage_types = [s.type for s in pipe.stages]
    assert "plan" in stage_types
    assert "implement" in stage_types
    assert pipe.target == "github"
    assert GHPipeline.STAGE_TYPES["plan"] is ghplan.GHPlan
    assert GHPipeline.STAGE_TYPES["implement"] is implement.Implement
    assert GHPipeline.STAGE_TYPES["commit"] is commit.Commit
    assert GHPipeline.STAGE_TYPES["open-github-pr"] is open_github_pr.OpenGitHubPR
    assert GHPipeline.STAGE_TYPES["request-copilot"] is request_copilot.RequestCopilot
    assert GHPipeline.STAGE_TYPES["ghreview"] is ghreview.GHReview
    assert GHPipeline.STAGE_TYPES["ghaddress"] is ghaddress.GHAddress
    assert GHPipeline.STAGE_TYPES["wait-ci"] is wait_ci.WaitCI
    assert GHPipeline.STAGE_TYPES["wait-copilot"] is wait_copilot.WaitCopilot


def test_local_pipeline_rejects_gh_stages(tmp_path):
    gh_stages = [
        StageEntry(
            name="commit", type="commit", client=None, prompt_paths=[], options={}
        )
    ]
    with pytest.raises(ValueError, match="commit"):
        _local(gh_stages, args=_args(), tmp_path=tmp_path)


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


def test_validate_resume_target_no_resume_from(tmp_path):
    pipe = _local(
        _make_stages("plan", "implement"),
        args=_args(resume_from=None),
        tmp_path=tmp_path,
    )
    pipe.validate_resume_target()  # should not raise


def test_validate_resume_target_valid_name(tmp_path):
    pipe = _local(
        _make_stages("plan", "implement"),
        args=_args(resume_from="implement"),
        tmp_path=tmp_path,
    )
    pipe.validate_resume_target()  # should not raise


def test_validate_resume_target_invalid_name(tmp_path):
    pipe = _local(
        _make_stages("plan", "implement"),
        args=_args(resume_from="bogus"),
        tmp_path=tmp_path,
    )
    with pytest.raises(ValueError, match="bogus"):
        pipe.validate_resume_target()


def test_validate_resume_target_parallel_group_name(tmp_path):
    pipe = _local(
        [_make_parallel_stage("reviews", ["review-a", "review-b"])],
        args=_args(resume_from="reviews"),
        tmp_path=tmp_path,
    )
    pipe.validate_resume_target()  # "reviews" is a valid expanded name


def test_validate_resume_target_parallel_fanout(tmp_path):
    pipe = _local(
        [_make_parallel_stage("reviews", ["review-a", "review-b"])],
        args=_args(resume_from="reviews-fanout"),
        tmp_path=tmp_path,
    )
    pipe.validate_resume_target()  # fanout is valid


def test_validate_resume_target_parallel_fanin(tmp_path):
    pipe = _local(
        [_make_parallel_stage("reviews", ["review-a", "review-b"])],
        args=_args(resume_from="reviews-fanin"),
        tmp_path=tmp_path,
    )
    pipe.validate_resume_target()  # fanin is valid


def test_validate_resume_target_child_name_rejected(tmp_path):
    pipe = _local(
        [_make_parallel_stage("reviews", ["review-a", "review-b"])],
        args=_args(resume_from="review-a"),
        tmp_path=tmp_path,
    )
    with pytest.raises(ValueError, match="review-a"):
        pipe.validate_resume_target()


def test_parallel_expansion_in_constructor(tmp_path):
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
