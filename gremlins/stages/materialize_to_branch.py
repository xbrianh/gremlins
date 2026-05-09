"""MaterializeToBranch stage: moves detached-HEAD commits onto a real branch."""

from __future__ import annotations

import dataclasses
import json
import sys
from typing import Any, cast

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
from gremlins.state import patch_state, resolve_state_file


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

        materialized_branch = ""
        if isinstance(outcome, HeadAdvanced):
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

        result = MaterializeToBranchResult(
            outcome=outcome,
            materialized_branch=materialized_branch,
            base_ref=pre_state.head,
        )
        patch_state(
            self.state.gr_id,
            impl_materialized_branch=result.materialized_branch,
            impl_base_ref=result.base_ref,
        )
        if materialized_branch and self.state.gr_id:
            self._record_child_branch(materialized_branch)
        return result

    def _record_child_branch(self, branch: str) -> None:
        sf = resolve_state_file(self.state.gr_id)
        if sf is None or not sf.exists():
            return
        try:
            data: dict[str, Any] = json.loads(sf.read_text(encoding="utf-8"))
            chain_st = data.get("chain_state")
            if not isinstance(chain_st, dict):
                return
            chain_st = cast(dict[str, Any], chain_st)
            n = int(chain_st.get("handoff_count", 0))
            records: list[dict[str, Any]] = list(chain_st.get("child_records") or [])
            for rec in records:
                if rec.get("n") == n:
                    rec["branch"] = branch
                    break
            else:
                records.append({"n": n, "branch": branch})
            chain_st["child_records"] = records
            patch_state(self.state.gr_id, chain_state=chain_st)
        except Exception:
            pass


register_stage("materialize-to-branch", MaterializeToBranch)
