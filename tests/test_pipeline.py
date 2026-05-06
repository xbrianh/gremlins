import argparse
import pathlib

import pytest

from gremlins.orchestrators.pipeline import GHPipeline, LocalPipeline
from gremlins.pipeline import StageEntry, resolve_pipeline_path
from gremlins.stages import (
    address_code,
    commit_pr,
    ghaddress,
    ghplan,
    ghreview,
    implement,
    plan,
    request_copilot,
    review_code,
    verify,
    wait_ci,
    wait_copilot,
)


def _args(**kwargs):
    return argparse.Namespace(**kwargs)


def test_local_pipeline_from_yaml(tmp_path, monkeypatch):
    monkeypatch.setattr(
        "gremlins.orchestrators.pipeline.resolve_session_dir",
        lambda gr_id=None: tmp_path,
    )
    pipe = LocalPipeline.from_yaml(
        resolve_pipeline_path("local", pathlib.Path.cwd()),
        args=_args(),
        gr_id=None,
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


def test_gh_pipeline_from_yaml(tmp_path, monkeypatch):
    monkeypatch.setattr(
        "gremlins.orchestrators.pipeline.resolve_session_dir",
        lambda gr_id=None: tmp_path,
    )
    pipe = GHPipeline.from_yaml(
        resolve_pipeline_path("gh", pathlib.Path.cwd()),
        args=_args(),
        gr_id=None,
    )

    assert len(pipe.stages) > 0
    assert all(isinstance(s, StageEntry) for s in pipe.stages)
    stage_types = [s.type for s in pipe.stages]
    assert "plan" in stage_types
    assert "implement" in stage_types
    assert pipe.target == "github"
    assert GHPipeline.STAGE_TYPES["plan"] is ghplan.GHPlan
    assert GHPipeline.STAGE_TYPES["implement"] is implement.Implement
    assert GHPipeline.STAGE_TYPES["commit-pr"] is commit_pr.CommitPR
    assert GHPipeline.STAGE_TYPES["request-copilot"] is request_copilot.RequestCopilot
    assert GHPipeline.STAGE_TYPES["ghreview"] is ghreview.GHReview
    assert GHPipeline.STAGE_TYPES["ghaddress"] is ghaddress.GHAddress
    assert GHPipeline.STAGE_TYPES["wait-ci"] is wait_ci.WaitCI
    assert GHPipeline.STAGE_TYPES["wait-copilot"] is wait_copilot.WaitCopilot


def test_local_pipeline_rejects_gh_stages(tmp_path):
    gh_stages = [StageEntry(name="commit-pr", type="commit-pr", client=None, prompt_paths=[], options={})]
    with pytest.raises(ValueError, match="commit-pr"):
        LocalPipeline(gh_stages, args=_args(), session_dir=tmp_path, gr_id=None)
