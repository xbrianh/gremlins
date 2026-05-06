"""Pipeline orchestrator classes."""

from __future__ import annotations

import argparse
import pathlib

from gremlins.git import in_git_repo
from gremlins.pipeline import StageEntry
from gremlins.pipeline import load_pipeline as _load_pipeline
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
from gremlins.stages.base import Stage
from gremlins.state import resolve_session_dir


class PipelineRunner:
    STAGE_TYPES: dict[str, type[Stage]] = {}
    target: str = ""

    def __init__(
        self,
        stages: list[StageEntry],
        *,
        args: argparse.Namespace,
        session_dir: pathlib.Path,
        gr_id: str | None,
    ) -> None:
        if self.STAGE_TYPES:
            unknown = [
                s.type
                for s in stages
                if s.type != "parallel" and s.type not in self.STAGE_TYPES
            ]
            if unknown:
                raise ValueError(
                    f"{type(self).__name__} does not support stage type(s): {unknown}"
                )
        self.stages = stages
        self.args = args
        self.session_dir = session_dir
        self.gr_id = gr_id
        self.is_git = in_git_repo()

    @classmethod
    def from_yaml(
        cls,
        path: pathlib.Path,
        *,
        args: argparse.Namespace,
        gr_id: str | None,
    ) -> "PipelineRunner":
        pipeline_data = _load_pipeline(path)
        session_dir = resolve_session_dir(gr_id)
        return cls(
            pipeline_data.stages,
            args=args,
            session_dir=session_dir,
            gr_id=gr_id,
        )


# Keep old name as alias for backwards compatibility during migration
Pipeline = PipelineRunner


class LocalPipeline(PipelineRunner):
    target = "local"
    STAGE_TYPES: dict[str, type[Stage]] = {
        "plan": plan.Plan,
        "implement": implement.Implement,
        "verify": verify.Verify,
        "review-code": review_code.ReviewCode,
        "address-code": address_code.AddressCode,
    }


class GHPipeline(PipelineRunner):
    target = "github"
    STAGE_TYPES: dict[str, type[Stage]] = {
        "plan": ghplan.GHPlan,
        "implement": implement.Implement,
        "verify": verify.Verify,
        "commit-pr": commit_pr.CommitPR,
        "request-copilot": request_copilot.RequestCopilot,
        "ghreview": ghreview.GHReview,
        "ghaddress": ghaddress.GHAddress,
        "wait-ci": wait_ci.WaitCI,
        "wait-copilot": wait_copilot.WaitCopilot,
    }
