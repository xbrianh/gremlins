"""MaterializeToBranch stage: moves detached-HEAD commits onto a real branch."""

from __future__ import annotations

import dataclasses
import json
import logging
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
from gremlins.stages import Stage, register_stage
from gremlins.state import append_artifact, patch_state, resolve_state_file

logger = logging.getLogger(__name__)


@dataclasses.dataclass
class MaterializeToBranchResult:
    outcome: ImplOutcome
    materialized_branch: str
    base_ref: str


class MaterializeToBranch(Stage):
    def _pre_state_from_file(self) -> PreImplState:
        sf = resolve_state_file(self.state.gr_id)
        if sf is None or not sf.exists():
            raise RuntimeError(
                "impl_pre_state not set and no state.json found; "
                "rewind to implement stage"
            )
        data: dict[str, Any] = json.loads(sf.read_text(encoding="utf-8"))
        head = data.get("impl_pre_head") or ""
        if not head:
            raise RuntimeError(
                "impl_pre_state not set and impl_pre_head missing from state.json; "
                "rewind to implement stage"
            )
        return PreImplState(head=head, branch=data.get("impl_pre_branch") or "")

    def run(self, pipe: Any) -> MaterializeToBranchResult:
        pre_state: PreImplState = (
            getattr(pipe, "impl_pre_state", None) or self._pre_state_from_file()
        )
        impl_cwd = str(self.state.worktree) if self.state.worktree is not None else None
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

        if not isinstance(outcome, HeadAdvanced):
            # DirtyOnly: uncommitted changes but no commits — nothing to hand off.
            # Don't overwrite impl_materialized_branch; leave any prior value intact.
            patch_state(self.state.gr_id, impl_base_ref=pre_state.head)
            return MaterializeToBranchResult(
                outcome=outcome,
                materialized_branch="",
                base_ref=pre_state.head,
            )

        materialized_branch = create_handoff_branch(pre_state, cwd=impl_cwd)
        reset_pre_branch(pre_state, cwd=impl_cwd)
        sweep_stale_handoff_branches(materialized_branch, cwd=impl_cwd)
        pre_branch_note = (
            f" and reset {pre_state.branch}" if pre_state.branch else ""
        )
        sys.stdout.write(
            f"    materialize-to-branch: moved {outcome.commit_count} commit(s) "
            f"onto {materialized_branch}{pre_branch_note}\n"
        )
        sys.stdout.flush()

        patch_state(
            self.state.gr_id,
            impl_materialized_branch=materialized_branch,
            impl_base_ref=pre_state.head,
        )
        append_artifact(self.state.gr_id, {"type": "branch", "name": materialized_branch})
        return MaterializeToBranchResult(
            outcome=outcome,
            materialized_branch=materialized_branch,
            base_ref=pre_state.head,
        )


register_stage("materialize-to-branch", MaterializeToBranch)
