"""MaterializeToBranch stage: moves detached-HEAD commits onto a real branch."""

from __future__ import annotations

import dataclasses
import logging
import sys

from gremlins.git import (
    DivergentHead,
    EmptyImpl,
    ImplOutcome,
    classify_impl_outcome,
    create_handoff_branch,
    reset_pre_branch,
    sweep_stale_handoff_branches,
)
from gremlins.stages.base import RuntimeState, Stage
from gremlins.stages.registry import register_stage
from gremlins.state import append_artifact, patch_state

logger = logging.getLogger(__name__)


@dataclasses.dataclass
class MaterializeToBranchResult:
    outcome: ImplOutcome
    materialized_branch: str
    base_ref: str


class MaterializeToBranch(Stage):
    def run(self, state: RuntimeState) -> MaterializeToBranchResult:
        pre_state = state.impl_pre_state
        if pre_state is None:
            raise RuntimeError(
                "no impl_pre_state found; rewind to implement stage"
            )
        impl_cwd = str(state.worktree) if state.worktree is not None else None
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

        materialized_branch = create_handoff_branch(pre_state, cwd=impl_cwd)
        reset_pre_branch(pre_state, cwd=impl_cwd)
        sweep_stale_handoff_branches(materialized_branch, cwd=impl_cwd)
        pre_branch_note = f" and reset {pre_state.branch}" if pre_state.branch else ""
        sys.stdout.write(
            f"    materialize-to-branch: moved {outcome.commit_count} commit(s) "
            f"onto {materialized_branch}{pre_branch_note}\n"
        )
        sys.stdout.flush()

        patch_state(
            state.gr_id,
            impl_materialized_branch=materialized_branch,
            impl_base_ref=pre_state.head,
        )
        append_artifact(state.gr_id, {"type": "branch", "name": materialized_branch})
        return MaterializeToBranchResult(
            outcome=outcome,
            materialized_branch=materialized_branch,
            base_ref=pre_state.head,
        )


register_stage("materialize-to-branch", MaterializeToBranch)
