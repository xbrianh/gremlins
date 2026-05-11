"""LoopStage: iterate body runners until head-stable or max iterations."""

from __future__ import annotations

import dataclasses
import json
import logging
from collections.abc import Callable
from typing import Any, cast

from gremlins.executor.state import State, resolve_state_file
from gremlins.stages.base import Stage
from gremlins.stages.registry import register_stage
from gremlins.utils import git as _git

logger = logging.getLogger(__name__)


class RunCmdFailed(Exception):
    """Raised to signal a failure that triggers the loop's body continuation.

    In verify: raised by RunCmd / _run_cmd when a check command exits non-zero,
    causing the fix runner to execute.  In handoff: raised when the handoff
    agent returns "next-plan", signalling the child pipeline runners to proceed.
    On a clean iteration (no RunCmdFailed), subsequent body runners are skipped.
    """


class LoopExhausted(RuntimeError):
    """Raised by LoopStage when max_iterations is reached without head-stable."""


class LoopStage(Stage):
    """Iterate body runners until HEAD is stable or max_iterations is reached.

    body_runners are called in order each iteration.  Subsequent runners only
    execute when a preceding runner raised RunCmdFailed — on a clean iteration
    all remaining runners are skipped.  Fix runners are also skipped on the
    final iteration even after a failure, so the stage bails without retrying.

    Termination: after an iteration where no RunCmdFailed was raised, if
    HEAD is unchanged (head-stable) the loop exits cleanly.  If HEAD
    advanced, the loop continues (or exits on the final iteration).

    Resume granularity: --resume-from targets the loop stage by name; there
    is no per-iteration sub-stage addressing.  Resuming restarts the loop from
    iteration 1, picking up persisted file-based state from session_dir (e.g.
    boss-spec.md, handoff-NNN.state.json).
    """

    type = "loop"

    def __init__(
        self,
        name: str,
        *,
        body: list[Stage] | None = None,
        body_runners: list[Callable[[], None]] | None = None,
        max_iterations: int,
        pr_stack: bool = False,
    ) -> None:
        super().__init__(name, None, [], {})
        self.body = body or []
        self._body_runners = body_runners
        self._max_iterations = max_iterations
        self._pr_stack = pr_stack

    @classmethod
    def from_runners(
        cls,
        runners: list[Callable[[], None]],
        *,
        name: str = "loop",
        max_iterations: int,
    ) -> LoopStage:
        return cls(name, body_runners=runners, max_iterations=max_iterations)

    @classmethod
    def with_dict(cls, d: dict[str, Any], depth: int = 0) -> LoopStage:
        from gremlins.pipeline.loader import get_client_from_dict, parse_stage

        raw_options: object = d.get("options") or {}
        if not isinstance(raw_options, dict):
            raise ValueError(f"stage {d['name']!r}: 'options' must be a mapping")
        options = cast(dict[str, Any], raw_options)
        max_iterations: int = int(options.get("max_iterations", 3))
        pr_stack: bool = bool(options.get("pr_stack", False))
        raw_children: object = d.get("body") or []
        if not isinstance(raw_children, list):
            raise ValueError(f"stage {d['name']!r}: 'body' must be a list")
        body = [
            parse_stage(child_d, depth=depth)
            for child_d in cast(list[dict[str, Any]], raw_children)
        ]
        stage = cls(
            d["name"], body=body, max_iterations=max_iterations, pr_stack=pr_stack
        )
        stage.client = get_client_from_dict(d)
        return stage

    def _build_runners(self, state: State) -> list[Callable[[], None]]:
        result: list[Callable[[], None]] = []
        for child in self.body:
            child_spec = state.stage_specs.get(child.name, state.client)
            if child.model is None:
                child.model = child_spec.model
            child_state = dataclasses.replace(
                state, client=state.get_client(child_spec)
            )
            result.append(child_state.make_runner(child, scope=self.body))
        return result

    def run(self, state: State) -> None:
        runners = (
            self._body_runners
            if self._body_runners is not None
            else self._build_runners(state)
        )
        exhausted = False
        try:
            for iteration in range(1, self._max_iterations + 1):
                state.patch(loop_iteration=iteration)
                if self._pr_stack:
                    _detach_to_pr_base(state)
                head_before = _git.head_sha(state.cwd)
                had_failure = False

                for i, runner in enumerate(runners):
                    if i > 0 and (not had_failure or iteration == self._max_iterations):
                        continue
                    try:
                        runner()
                    except RunCmdFailed:
                        had_failure = True

                if not had_failure:
                    head_after = _git.head_sha(state.cwd)
                    if head_after == head_before:
                        return  # head-stable termination
                    logger.info(
                        "loop iteration %d: HEAD advanced, continuing", iteration
                    )
                    if iteration == self._max_iterations:
                        return  # checks passed; accept even if HEAD advanced
                elif iteration == self._max_iterations:
                    break

            exhausted = True
            state.emit_bail(
                "other",
                f"loop exhausted {self._max_iterations} iterations",
            )
            raise LoopExhausted(f"loop exhausted {self._max_iterations} iterations")
        except LoopExhausted:
            raise
        except (SystemExit, Exception) as exc:
            if not exhausted and not _bail_already_set(state.gr_id, state.child_key):
                state.emit_bail(
                    "other",
                    f"loop stage failed: {exc}"[:200],
                )
            raise


def _detach_to_pr_base(state: State) -> None:
    branch = state.last_pr_branch()
    if not branch:
        return
    logger.info("detaching worktree to previous PR branch: %s", branch)
    _git.git_detach_to_branch(branch, cwd=state.cwd)


def _bail_already_set(gr_id: str | None, child_key: str | None) -> bool:
    sf = resolve_state_file(gr_id)
    if sf is None or not sf.exists():
        return False
    try:
        data: dict[str, Any] = json.loads(sf.read_text(encoding="utf-8"))
        if child_key is not None:
            pb: dict[str, Any] = data.get("parallel_bails") or {}
            shard: dict[str, Any] = pb.get(child_key) or {}
            return bool(shard.get("bail_class"))
        return bool(data.get("bail_class"))
    except Exception:
        return False


register_stage("loop", LoopStage)
