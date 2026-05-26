from __future__ import annotations

from typing import Any

from gremlins.stages.agent import Agent
from gremlins.stages.base import Stage
from gremlins.stages.exec import Exec
from gremlins.stages.github_open_pull_request import GitHubOpenPullRequest
from gremlins.stages.github_request_copilot_review import GitHubRequestCopilotReview
from gremlins.stages.github_wait_ci import GitHubWaitCI
from gremlins.stages.github_wait_copilot import GitHubWaitCopilot
from gremlins.stages.handoff import Handoff
from gremlins.stages.loop import LoopStage
from gremlins.stages.parallel import ParallelStage
from gremlins.stages.plan import Plan
from gremlins.stages.review_code import ReviewCode
from gremlins.stages.sequence import SequenceStage
from gremlins.stages.verify import Verify

STAGE_TYPES: dict[str, type[Stage]] = {
    "agent": Agent,
    "plan": Plan,
    "verify": Verify,
    "github-open-pull-request": GitHubOpenPullRequest,
    "github-request-copilot-review": GitHubRequestCopilotReview,
    "github-wait-copilot": GitHubWaitCopilot,
    "github-wait-ci": GitHubWaitCI,
    "review-code": ReviewCode,
    "handoff": Handoff,
    "loop": LoopStage,
    "parallel": ParallelStage,
    "sequence": SequenceStage,
    "exec": Exec,
}


def fill_names(raw_stages: list[dict[str, Any]]) -> None:
    """Fill missing 'name' fields in-place; append -N suffix on collisions."""
    explicit: set[str] = {
        d["name"] for d in raw_stages if isinstance(d.get("name"), str) and d["name"]
    }
    used: set[str] = set(explicit)
    counts: dict[str, int] = {}
    for d in raw_stages:
        if isinstance(d.get("name"), str) and d["name"]:
            continue
        stage_type = "parallel" if "parallel" in d else str(d.get("type") or "")
        counts[stage_type] = counts.get(stage_type, 0) + 1
        n = counts[stage_type]
        candidate = stage_type if n == 1 else f"{stage_type}-{n}"
        while candidate in used:
            n += 1
            candidate = f"{stage_type}-{n}"
        counts[stage_type] = n
        d["name"] = candidate
        used.add(candidate)


def parse_stages(raw: list[dict[str, Any]], depth: int = 0) -> list[Stage]:
    fill_names(raw)
    return [parse_stage(d, depth=depth) for d in raw]


def parse_stage(d: dict[str, Any], depth: int = 0) -> Stage:
    if "parallel" in d:
        stage = ParallelStage.with_dict(d, depth=depth)
        stage.raw_dict = d
        return stage

    name = d.get("name") or ""
    if "max_concurrent" in d:
        raise ValueError(
            f"stage {name!r}: 'max_concurrent' is only valid on parallel groups"
        )
    stage_type = d.get("type")
    if not isinstance(stage_type, str) or not stage_type:
        raise ValueError(f"stage {name!r}: must have a 'type' field")
    if stage_type not in STAGE_TYPES:
        raise ValueError(f"stage {name!r}: unknown type {stage_type!r}")
    stage = STAGE_TYPES[stage_type].with_dict(d, depth=depth)
    stage.raw_dict = d
    return stage
