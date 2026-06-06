"""LoopStage: iterate body runners until termination predicate or max iterations."""

from __future__ import annotations

import asyncio
import logging
import pathlib
from collections.abc import Awaitable, Callable
from typing import TYPE_CHECKING, Any, cast

from gremlins.artifacts.registry import ArtifactRegistry
from gremlins.stages.base import Stage, get_client_from_dict
from gremlins.stages.composite import child_state as _child_state
from gremlins.stages.outcome import Bail, Done, Outcome
from gremlins.utils import git as _git

if TYPE_CHECKING:
    from gremlins.executor.gremlin import Gremlin

logger = logging.getLogger(__name__)

# Called after a clean (no marker) iteration; returns True to exit the loop.
UntilFn = Callable[["Gremlin", int, str], bool]

_MARKER_KEY = "status"
_MARKER_VALUE = "needs_fix"
_BAIL_KEY = "bail"


def _is_marker_set(artifacts: ArtifactRegistry) -> bool:
    return (
        artifacts.produced(_MARKER_KEY)
        and artifacts.read(_MARKER_KEY).strip() == _MARKER_VALUE
    )


def _is_bail_set(artifacts: ArtifactRegistry) -> bool:
    return artifacts.produced(_BAIL_KEY)


def _do_bail(gremlin: Gremlin, artifacts: ArtifactRegistry) -> None:
    if gremlin.state is None:
        raise RuntimeError("gremlin.state is required for _do_bail")
    reason = str(artifacts.read(_BAIL_KEY)).strip()
    gremlin.state.record_bail(reason)
    raise Bail(reason)


def head_stable(gremlin: Gremlin, iteration: int, head_before: str) -> bool:
    """Exit when HEAD hasn't changed across this iteration."""
    if gremlin.state is None:
        raise RuntimeError("gremlin.state is required for head_stable")
    state = gremlin.state
    return _git.head_sha(pathlib.Path(state.cwd)) == head_before


def max_iters(n: int) -> UntilFn:
    """Exit after n clean iterations regardless of HEAD movement."""
    return lambda _state, iteration, _head: iteration >= n


async def _dispatch_runners(
    runners: list[Callable[[], Awaitable[Outcome]]],
    iteration: int,
    max_iterations: int,
    artifacts: ArtifactRegistry,
) -> bool:
    had_failure = False
    for i, runner in enumerate(runners):
        if i > 0 and (not had_failure or iteration == max_iterations):
            continue
        await runner()
        if _is_bail_set(artifacts):
            return had_failure
        if not had_failure and _is_marker_set(artifacts):
            had_failure = True
    return had_failure


class LoopStage(Stage):
    """Iterate body runners until a termination predicate fires or max_iterations is reached.

    Body runners execute in order each iteration. Subsequent runners only run
    when a preceding runner set the status=needs_fix marker artifact — on a
    clean iteration all remaining runners are skipped. Fix runners are also
    skipped on the final iteration so the stage bails without retrying.

    Resume granularity: resuming targets the loop by name; resuming
    restarts from iteration 1, picking up file-based state from artifact_dir.
    """

    type = "loop"

    def __init__(
        self,
        name: str,
        *,
        body: list[Stage] | None = None,
        body_runners: list[Callable[[], Awaitable[Outcome]]] | None = None,
        max_iterations: int,
        until: UntilFn = head_stable,
        interval: float | None = None,
    ) -> None:
        super().__init__(name)
        self.body = body or []
        for c in self.body:
            c.path = f"{name}/{c.name}"
        self._body_runners = body_runners
        self._max_iterations = max_iterations
        self._until = until
        self._interval = interval

    @classmethod
    def with_dict(cls, d: dict[str, Any], depth: int = 0) -> LoopStage:
        from gremlins.pipeline.loader import parse_stages

        name = d.get("name") or ""
        raw_options: object = d.get("options") or {}
        if not isinstance(raw_options, dict):
            raise ValueError(f"stage {name!r}: 'options' must be a mapping")
        options = cast(dict[str, Any], raw_options)
        max_iterations: int = int(
            d.get("max-iterations") or options.get("max_iterations", 3)
        )
        raw_interval = options.get("interval")
        interval: float | None = (
            float(raw_interval) if raw_interval is not None else None
        )

        raw_children: object = d.get("body") or []
        if not isinstance(raw_children, list):
            raise ValueError(f"stage {name!r}: 'body' must be a list")

        body = parse_stages(cast(list[dict[str, Any]], raw_children), depth=depth)
        stage = cls(
            name,
            body=body,
            max_iterations=max_iterations,
            interval=interval,
        )
        stage.client = get_client_from_dict(d)
        return stage

    def _build_runners(
        self, gremlin: Gremlin
    ) -> list[Callable[[], Awaitable[Outcome]]]:
        if gremlin.state is None:
            raise RuntimeError("gremlin.state is required for _build_runners")
        state = gremlin.state
        result: list[Callable[[], Awaitable[Outcome]]] = []
        for child in self.body:
            cs = _child_state(state, child)
            base: Callable[[], Awaitable[Any]] = cs.make_runner(
                child, gremlin, scope=self.body, record_stage=False
            )
            name = child.name

            async def _tracked(
                r: Callable[[], Awaitable[Any]] = base, n: str = name
            ) -> Outcome:
                state.data.patch(active_children=[n])
                try:
                    return cast(Outcome, await r())
                finally:
                    state.data.patch(_delete=("active_children",))

            result.append(cast(Callable[[], Awaitable[Outcome]], _tracked))
        return result

    async def run(self, gremlin: Gremlin) -> Outcome:
        if gremlin.state is None:
            raise RuntimeError("gremlin.state is required for LoopStage")
        for iteration in range(1, self._max_iterations + 1):
            gremlin.state.record_state_field(loop_iteration=iteration)
            gremlin.state.artifacts.unbind(_MARKER_KEY)
            gremlin.state.artifacts.unbind(_BAIL_KEY)
            for child in self.body:
                for key in getattr(child, "out_map", {}):
                    gremlin.state.artifacts.unbind(key)
            head_before = _git.head_sha(pathlib.Path(gremlin.state.cwd))
            runners = (
                self._body_runners
                if self._body_runners is not None
                else self._build_runners(gremlin)
            )
            had_failure = await _dispatch_runners(
                runners, iteration, self._max_iterations, gremlin.state.artifacts
            )
            if _is_bail_set(gremlin.state.artifacts):
                _do_bail(gremlin, gremlin.state.artifacts)

            if not had_failure:
                if self._until(gremlin, iteration, head_before):
                    return Done()
                logger.info("loop iteration %d: continuing", iteration)
                if iteration == self._max_iterations:
                    return Done()
            elif iteration == self._max_iterations:
                break
            if self._interval is not None:
                await asyncio.sleep(self._interval)

        gremlin.state.record_bail(f"loop exhausted {self._max_iterations} iterations")
        raise Bail(f"loop exhausted {self._max_iterations} iterations")
