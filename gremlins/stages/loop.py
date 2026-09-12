"""LoopStage: iterate body runners until termination predicate or max iterations."""

from __future__ import annotations

import asyncio
import logging
import pathlib
from collections.abc import Awaitable, Callable
from typing import TYPE_CHECKING, Any, cast

from _gremlins_core.artifacts import ArtifactRegistry
from _gremlins_core.stages import (
    _BAIL_KEY,
    Bail,
    Done,
    Outcome,
    StageAttrs,
    get_client_from_dict,
)
from _gremlins_core.stages import child_state as _child_state

if TYPE_CHECKING:
    from gremlins.executor.gremlin import Gremlin

logger = logging.getLogger(__name__)


def _bail_reason(artifacts: ArtifactRegistry, key: str) -> str | None:
    """Bail reason bound to *key*, or None when unset or already cleared.

    Registry membership alone is not enough for file-backed bail entries: the
    loop clears a stale per-iteration bail by unlinking its file on resume, so
    a missing file means the bail no longer applies.
    """
    if not artifacts.is_registered(key):
        return None
    raw = artifacts.data_uri(key)
    if not (isinstance(raw, str) and raw.startswith("/")):
        return str(raw).strip()
    path = pathlib.Path(raw)
    if not path.exists():
        return None
    try:
        return path.read_text(encoding="utf-8").strip()
    except (OSError, ValueError):
        return raw


def _is_bail_set(artifacts: ArtifactRegistry, loop_iter: str) -> bool:
    return (
        _bail_reason(artifacts, f"artifact://{loop_iter}/bail") is not None
        or _bail_reason(artifacts, _BAIL_KEY) is not None
    )


def _do_bail(gremlin: Gremlin, artifacts: ArtifactRegistry, loop_iter: str) -> None:
    if gremlin.state is None:
        raise RuntimeError("gremlin.state is required for _do_bail")
    reason = _bail_reason(artifacts, f"artifact://{loop_iter}/bail")
    if reason is None:
        reason = _bail_reason(artifacts, _BAIL_KEY) or ""
    gremlin.state.record_bail(reason)
    raise Bail(reason)


class LoopStage(StageAttrs):
    """Iterate body stages until max_iterations or a stop condition is met.

    Body stages execute in order every iteration. After each full body run:
    - if the bail artifact is set, the loop raises Bail
    - if the stop_when_exists artifact is bound, the loop returns Done
    - if max_iterations is reached without stopping, the loop raises Bail

    The pipeline YAML defines the stopping condition explicitly via stop_when_exists.

    Resume granularity: resuming targets the loop by name; resuming
    restarts from iteration 1, picking up file-based state from artifact_dir.
    """

    type = "loop"

    def __init__(
        self,
        name: str,
        *,
        body: list[Any] | None = None,
        body_runners: list[Callable[[], Awaitable[Outcome]]] | None = None,
        max_iterations: int,
        stop_when_exists: str | None = None,
        interval: float | None = None,
    ) -> None:
        super().__init__(name)
        self.body = body or []
        for c in self.body:
            c.path = f"{name}/{c.name}"
        if max_iterations < 1:
            raise ValueError(
                f"LoopStage {self.name!r}: max_iterations must be >= 1, got {max_iterations}"
            )
        self._body_runners = body_runners
        self._max_iterations = max_iterations
        self._stop_when_exists = stop_when_exists
        self._interval = interval

    @classmethod
    def with_dict(cls, d: dict[str, Any], depth: int = 0) -> LoopStage:
        from _gremlins_core.schemas import parse_stages

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
        stop_when_exists: str | None = d.get("stop_when_exists")

        raw_children: object = d.get("body") or []
        if not isinstance(raw_children, list):
            raise ValueError(f"stage {name!r}: 'body' must be a list")

        body = parse_stages(raw_children, depth=depth)
        stage = cls(
            name,
            body=body,
            max_iterations=max_iterations,
            stop_when_exists=stop_when_exists,
            interval=interval,
        )
        client = get_client_from_dict(d)
        stage.client = client
        stage.client_explicit = client is not None
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
        state = gremlin.state
        state.push_loop(self.path or self.name)
        try:
            if logger.isEnabledFor(logging.DEBUG):
                logger.debug(
                    "loop %s: starting (max_iterations=%d, stop_when_exists=%s, interval=%s)",
                    self.name,
                    self._max_iterations,
                    self._stop_when_exists,
                    self._interval,
                )
            for iteration in range(1, self._max_iterations + 1):
                state.set_loop_iteration(iteration)

                # Clear any stale per-iteration bail artifact from a prior
                # attempt/resume so it doesn't pollute the current iteration.
                scoped_bail = f"artifact://{state.loop_iter}/bail"
                if state.artifacts.is_registered(scoped_bail):
                    bail_path = state.artifacts.data_uri(scoped_bail)
                    if isinstance(bail_path, str):
                        try:
                            pathlib.Path(bail_path).unlink(missing_ok=True)
                        except OSError:
                            pass

                logger.info(
                    "loop %s: iteration %d/%d starting (%d body runners)",
                    self.name,
                    iteration,
                    self._max_iterations,
                    len(self.body),
                )
                runners = (
                    self._body_runners
                    if self._body_runners is not None
                    else self._build_runners(gremlin)
                )
                for runner in runners:
                    await runner()

                if _is_bail_set(state.artifacts, state.loop_iter):
                    logger.info(
                        "loop %s: iteration %d hit bail artifact",
                        self.name,
                        iteration,
                    )
                    _do_bail(gremlin, state.artifacts, state.loop_iter)

                if self._stop_when_exists is not None:
                    resolved = self._stop_when_exists.replace(
                        "{loop_iter}", state.loop_iter
                    )
                    if state.artifacts.is_live(resolved) or state.artifacts.is_live(
                        f"artifact://{resolved}"
                    ):
                        logger.info(
                            "loop %s: stopped after %d iteration(s) — artifact %r produced",
                            self.name,
                            iteration,
                            self._stop_when_exists,
                        )
                        return Done()

                if iteration == self._max_iterations:
                    logger.info(
                        "loop %s: exhausted after %d iteration(s) without meeting stop condition",
                        self.name,
                        self._max_iterations,
                    )
                    state.record_bail(
                        f"loop exhausted {self._max_iterations} iterations"
                    )
                    raise Bail(f"loop exhausted {self._max_iterations} iterations")

                if self._interval is not None:
                    await asyncio.sleep(self._interval)

            # All loop paths above either return Done() or raise Bail.
            raise RuntimeError(
                f"LoopStage.run() fell through — "
                f"max_iterations={self._max_iterations}, "
                f"stop_when_exists={self._stop_when_exists!r}"
            )
        finally:
            state.pop_loop()
