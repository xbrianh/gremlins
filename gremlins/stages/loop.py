"""LoopStage: iterate body runners until termination predicate or max iterations."""

from __future__ import annotations

import inspect
import logging
from collections.abc import Callable
from typing import Any, cast

from gremlins.executor.state import State
from gremlins.stages.base import Stage
from gremlins.stages.composite import child_state as _child_state
from gremlins.stages.outcome import Bail, Done, NeedsFix, Outcome
from gremlins.utils import git as _git

logger = logging.getLogger(__name__)

# Called after a clean (no NeedsFix) iteration; returns True to exit the loop.
UntilFn = Callable[[State, int, str], bool]


def head_stable(state: State, iteration: int, head_before: str) -> bool:
    """Exit when HEAD hasn't changed across this iteration."""
    return _git.head_sha(state.cwd) == head_before


def max_iters(n: int) -> UntilFn:
    """Exit after n clean iterations regardless of HEAD movement."""
    return lambda _state, iteration, _head: iteration >= n


async def _dispatch_runners(
    runners: list[Callable[[], Outcome]],
    iteration: int,
    max_iterations: int,
) -> bool:
    had_failure = False
    for i, runner in enumerate(runners):
        if i > 0 and (not had_failure or iteration == max_iterations):
            continue
        if inspect.iscoroutinefunction(runner):
            outcome = await runner()
        else:
            outcome = runner()
        if isinstance(outcome, NeedsFix):
            had_failure = True
    return had_failure


class LoopStage(Stage):
    """Iterate body runners until a termination predicate fires or max_iterations is reached.

    Body runners execute in order each iteration. Subsequent runners only run
    when a preceding runner returned NeedsFix — on a clean iteration all
    remaining runners are skipped. Fix runners are also skipped on the final
    iteration so the stage bails without retrying.

    Resume granularity: --resume-from targets the loop by name; resuming
    restarts from iteration 1, picking up file-based state from session_dir.
    """

    type = "loop"

    def __init__(
        self,
        name: str,
        *,
        body: list[Stage] | None = None,
        body_runners: list[Callable[[], Outcome]] | None = None,
        max_iterations: int,
        until: UntilFn = head_stable,
        on_iteration_start: Callable[[State], None] | None = None,
    ) -> None:
        super().__init__(name)
        self.body = body or []
        for c in self.body:
            c.path = f"{name}/{c.name}"
        self._body_runners = body_runners
        self._max_iterations = max_iterations
        self._until = until
        self._on_iteration_start = on_iteration_start

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
        on_iter = detach_to_pr_base if pr_stack else None
        stage = cls(
            d["name"],
            body=body,
            max_iterations=max_iterations,
            on_iteration_start=on_iter,
        )
        stage.client = get_client_from_dict(d)
        return stage

    def _build_runners(self, state: State) -> list[Callable[[], Outcome]]:
        result: list[Callable[[], Outcome]] = []
        for child in self.body:
            cs = _child_state(state, child)
            runner = cs.make_runner(child, scope=self.body, record_stage=False)
            result.append(cast(Callable[[], Outcome], runner))
        return result

    async def run(self, state: State) -> Outcome:
        runners = (
            self._body_runners
            if self._body_runners is not None
            else self._build_runners(state)
        )
        for iteration in range(1, self._max_iterations + 1):
            state.record_state_field(loop_iteration=iteration)
            if self._on_iteration_start:
                self._on_iteration_start(state)
            head_before = _git.head_sha(state.cwd)
            had_failure = await _dispatch_runners(
                runners, iteration, self._max_iterations
            )

            if not had_failure:
                if self._until(state, iteration, head_before):
                    return Done()
                logger.info("loop iteration %d: continuing", iteration)
                if iteration == self._max_iterations:
                    return Done()
            elif iteration == self._max_iterations:
                break

        state.record_bail(f"loop exhausted {self._max_iterations} iterations")
        raise Bail(f"loop exhausted {self._max_iterations} iterations")


def detach_to_pr_base(state: State) -> None:
    branch = state.data.last_pr_branch()
    if not branch:
        return
    logger.info("detaching worktree to previous PR branch: %s", branch)
    _git.git_detach_to_branch(branch, cwd=state.cwd)
