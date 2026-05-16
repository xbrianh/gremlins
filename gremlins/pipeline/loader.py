from __future__ import annotations

from typing import Any

from gremlins.clients.client import Client
from gremlins.stages.address_code import AddressCode, GitHubAddressPullRequestReviews
from gremlins.stages.base import Stage
from gremlins.stages.cmd import Cmd
from gremlins.stages.github_open_pull_request import GitHubOpenPullRequest
from gremlins.stages.github_request_copilot_review import GitHubRequestCopilotReview
from gremlins.stages.github_wait_ci import GitHubWaitCI
from gremlins.stages.github_wait_copilot import GitHubWaitCopilot
from gremlins.stages.handoff import Handoff
from gremlins.stages.implement import Implement
from gremlins.stages.loop import LoopStage
from gremlins.stages.parallel import ParallelStage
from gremlins.stages.plan import Plan
from gremlins.stages.review_code import GitHubReviewPullRequest, ReviewCode
from gremlins.stages.sequence import SequenceStage
from gremlins.stages.verify import Verify

STAGE_TYPES: dict[str, type[Stage]] = {
    "plan": Plan,
    "implement": Implement,
    "verify": Verify,
    "github-open-pull-request": GitHubOpenPullRequest,
    "github-request-copilot-review": GitHubRequestCopilotReview,
    "github-wait-copilot": GitHubWaitCopilot,
    "github-wait-ci": GitHubWaitCI,
    "review-code": ReviewCode,
    "github-review-pull-request": GitHubReviewPullRequest,
    "address-code": AddressCode,
    "github-address-pull-request-reviews": GitHubAddressPullRequestReviews,
    "handoff": Handoff,
    "loop": LoopStage,
    "parallel": ParallelStage,
    "sequence": SequenceStage,
    "cmd": Cmd,
}


def get_client_from_dict(d: dict[str, Any]) -> Client | None:
    raw = d.get("client")
    if raw is None:
        return None
    if not isinstance(raw, str):
        raise ValueError(
            f"stage {d.get('name', '?')!r}: 'client' must be a string, got {type(raw)!r}"
        )
    return Client.parse(raw)


def parse_stage(d: dict[str, Any], depth: int = 0) -> Stage:
    if "parallel" in d:
        return ParallelStage.with_dict(d, depth=depth)

    name = d.get("name")
    if not isinstance(name, str) or not name:
        raise ValueError("stage entry must have a 'name' field")
    if "max_concurrent" in d:
        raise ValueError(
            f"stage {name!r}: 'max_concurrent' is only valid on parallel groups"
        )
    stage_type = d.get("type")
    if not isinstance(stage_type, str) or not stage_type:
        raise ValueError(f"stage {name!r}: must have a 'type' field")
    if stage_type not in STAGE_TYPES:
        raise ValueError(f"stage {name!r}: unknown type {stage_type!r}")
    return STAGE_TYPES[stage_type].with_dict(d, depth=depth)
