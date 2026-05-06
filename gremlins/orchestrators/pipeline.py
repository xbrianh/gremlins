"""Pipeline orchestrator classes."""

from __future__ import annotations

import argparse
import dataclasses
import pathlib

from gremlins.clients.protocol import ClaudeClient
from gremlins.git import in_git_repo
from gremlins.pipeline import StageEntry
from gremlins.pipeline import load_pipeline as _load_pipeline
from gremlins.runner import install_signal_handlers
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


def _expand_stage_entries(raw_stages: list[StageEntry]) -> list[StageEntry]:
    top_level_names = {e.name for e in raw_stages}
    child_names: set[str] = set()
    seen: set[str] = set()
    result: list[StageEntry] = []

    for entry in raw_stages:
        if entry.type == "parallel":
            for child in entry.children:
                if child.name in child_names or child.name in top_level_names:
                    raise ValueError(f"duplicate child stage name {child.name!r}")
                child_names.add(child.name)
            # children are not resume targets; only the three group-level stages are
            for name, typ in [
                (f"{entry.name}-fanout", "parallel-fanout"),
                (entry.name, "parallel-group"),
                (f"{entry.name}-fanin", "parallel-fanin"),
            ]:
                if name in seen:
                    raise ValueError(f"pipeline has duplicate stage name {name!r}")
                seen.add(name)
                result.append(dataclasses.replace(entry, name=name, type=typ))
        else:
            if entry.name in seen:
                raise ValueError(f"pipeline has duplicate stage name {entry.name!r}")
            seen.add(entry.name)
            result.append(entry)

    return result


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
        self.stages = _expand_stage_entries(stages)
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

    def validate_resume_target(self) -> None:
        resume_from = getattr(self.args, "resume_from", None)
        if not resume_from:
            return
        valid_names = [entry.name for entry in self.stages]
        if resume_from not in valid_names:
            raise ValueError(
                f"--resume-from {resume_from!r} is not a valid stage; "
                f"valid: {valid_names}"
            )

    def run(self, *clients: ClaudeClient) -> None:
        # stub — stage-running loop lands in a later plan step
        install_signal_handlers(*clients)


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
