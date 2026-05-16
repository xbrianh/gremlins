"""LoopStage: iterate body runners until head-stable or max iterations."""

from __future__ import annotations

import dataclasses
import logging
from collections.abc import Callable
from typing import Any, cast

from gremlins.executor.state import State, resolve_state_file
from gremlins.stages.base import Stage
from gremlins.stages.outcome import Bail, Done, NeedsFix, Outcome
from gremlins.utils import git as _git

logger = logging.getLogger(__name__)


class LoopStage(Stage):
    """Iterate body runners until HEAD is stable or max_iterations is reached.

    body_runners are called in order each iteration.  Subsequent runners only
    execute when a preceding runner returned NeedsFix — on a clean iteration
    all remaining runners are skipped.  Fix runners are also skipped on the
    final iteration even after a failure, so the stage bails without retrying.

    Termination: after an iteration where no NeedsFix was returned, if
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
        body_runners: list[Callable[[], Outcome]] | None = None,
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
        runners: list[Callable[[], Outcome]],
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

    def _build_runners(self, state: State) -> list[Callable[[], Outcome]]:
        result: list[Callable[[], Outcome]] = []
        for child in self.body:
            child_state = dataclasses.replace(
                state, client=state.test_client or child.client
            )
            runner = child_state.make_runner(child, scope=self.body)
            result.append(cast(Callable[[], Outcome], runner))
        return result

    def run(self, state: State) -> Outcome:
        runners = (
            self._body_runners
            if self._body_runners is not None
            else self._build_runners(state)
        )
        try:
            for iteration in range(1, self._max_iterations + 1):
                state.data.patch(loop_iteration=iteration)
                if self._pr_stack:
                    _detach_to_pr_base(state)
                head_before = _git.head_sha(state.cwd)
                had_failure = False

                for i, runner in enumerate(runners):
                    if i > 0 and (not had_failure or iteration == self._max_iterations):
                        continue
                    outcome = runner()
                    if isinstance(outcome, Bail):
                        return outcome
                    if isinstance(outcome, NeedsFix):
                        had_failure = True

                if not had_failure:
                    head_after = _git.head_sha(state.cwd)
                    if head_after == head_before:
                        return Done()
                    logger.info(
                        "loop iteration %d: HEAD advanced, continuing", iteration
                    )
                    if iteration == self._max_iterations:
                        return Done()
                elif iteration == self._max_iterations:
                    break

            state.data.write_bail_file(
                "other",
                f"loop exhausted {self._max_iterations} iterations",
                attempt=state.data.attempt,
            )
            return Bail(f"loop exhausted {self._max_iterations} iterations")
        except (SystemExit, Exception) as exc:
            if not _bail_file_exists(state.data.gremlin_id, state.data.attempt):
                state.data.write_bail_file(
                    "other",
                    f"loop stage failed: {exc}"[:200],
                    attempt=state.data.attempt,
                )
            raise


def _detach_to_pr_base(state: State) -> None:
    branch = state.data.last_pr_branch()
    if not branch:
        return
    logger.info("detaching worktree to previous PR branch: %s", branch)
    _git.git_detach_to_branch(branch, cwd=state.cwd)


def _bail_file_exists(gremlin_id: str | None, attempt: str) -> bool:
    sf = resolve_state_file(gremlin_id)
    if sf is None or not sf.exists() or not attempt:
        return False
    return (sf.parent / f"bail_{attempt}.json").exists()
