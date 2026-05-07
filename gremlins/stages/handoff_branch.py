"""HandoffBranch stage: manages the git handoff-branch lifecycle after implement."""

from __future__ import annotations

import dataclasses
import sys
from typing import Any

from gremlins.git import (
    DivergentHead,
    EmptyImpl,
    HeadAdvanced,
    ImplOutcome,
    PreImplState,
    classify_impl_outcome,
    create_handoff_branch,
    reset_pre_branch,
    sweep_stale_handoff_branches,
)
from gremlins.stages.base import Stage
from gremlins.stages.registry import register_stage


@dataclasses.dataclass
class HandoffBranchResult:
    outcome: ImplOutcome
    handoff_branch: str
    base_ref: str


class HandoffBranch(Stage):
    def run(self, pipe: Any) -> HandoffBranchResult:
        if pipe.impl_pre_state is None:
            raise RuntimeError(
                "impl_pre_state not set; cannot run handoff-branch without implement "
                "(rewind to implement stage)"
            )
        impl_cwd = str(self.state.worktree) if self.state.worktree is not None else None
        pre_state: PreImplState = pipe.impl_pre_state
        outcome = classify_impl_outcome(pre_state, cwd=impl_cwd)

        if isinstance(outcome, EmptyImpl):
            raise RuntimeError(
                "implementation step produced no changes; refusing to open empty PR"
            )
        if isinstance(outcome, DivergentHead):
            raise RuntimeError(
                f"implementation changed HEAD from {outcome.pre_head} to {outcome.post_head} "
                "without advancing from the starting commit; refusing to treat this as "
                "committed work to hand off"
            )

        handoff_branch = ""
        if isinstance(outcome, HeadAdvanced):
            handoff_branch = create_handoff_branch(pre_state, cwd=impl_cwd)
            reset_pre_branch(pre_state, cwd=impl_cwd)
            sweep_stale_handoff_branches(handoff_branch, cwd=impl_cwd)
            pre_branch_note = (
                f" and reset {pre_state.branch}" if pre_state.branch else ""
            )
            sys.stdout.write(
                f"    handoff-branch: moved {outcome.commit_count} commit(s) "
                f"onto {handoff_branch}{pre_branch_note}\n"
            )
            sys.stdout.flush()

        return HandoffBranchResult(
            outcome=outcome,
            handoff_branch=handoff_branch,
            base_ref=pre_state.head,
        )


register_stage("handoff-branch", HandoffBranch)
