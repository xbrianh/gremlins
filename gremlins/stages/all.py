"""Import all stage modules so each self-registers into the stage registry."""

from __future__ import annotations

import gremlins.stages.address_code as address_code
import gremlins.stages.claude_prompt as claude_prompt
import gremlins.stages.commit as commit
import gremlins.stages.commit_pr as commit_pr
import gremlins.stages.handoff as handoff_stage_mod
import gremlins.stages.implement as implement
import gremlins.stages.loop as loop
import gremlins.stages.open_github_pr as open_github_pr
import gremlins.stages.parallel as parallel
import gremlins.stages.plan as plan
import gremlins.stages.request_copilot as request_copilot
import gremlins.stages.review_code as review_code
import gremlins.stages.run_cmd as run_cmd
import gremlins.stages.sequence as sequence
import gremlins.stages.verify as verify
import gremlins.stages.wait_ci as wait_ci
import gremlins.stages.wait_copilot as wait_copilot

__all__ = [
    "address_code",
    "claude_prompt",
    "commit",
    "commit_pr",
    "handoff_stage_mod",
    "implement",
    "loop",
    "open_github_pr",
    "parallel",
    "plan",
    "request_copilot",
    "review_code",
    "run_cmd",
    "sequence",
    "verify",
    "wait_ci",
    "wait_copilot",
]
