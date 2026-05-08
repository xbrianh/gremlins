"""LoopStage: iterate body runners until head-stable or max iterations."""

from __future__ import annotations

import json
import logging
from collections.abc import Callable
from typing import TYPE_CHECKING, Any

from gremlins import git as _git
from gremlins.stages.compound import CompoundStage
from gremlins.stages.registry import register_stage
from gremlins.state import emit_bail

if TYPE_CHECKING:
    from gremlins.pipeline import StageEntry

logger = logging.getLogger(__name__)


class RunCmdFailed(Exception):
    """Raised by RunCmd (or verify's _run_cmd closure) on non-zero exit."""


class LoopExhausted(RuntimeError):
    """Raised by LoopStage when max_iterations is reached without head-stable."""


class LoopStage(CompoundStage):
    """Iterate body runners until HEAD is stable or max_iterations is reached.

    body_runners are called in order each iteration.  Subsequent runners only
    execute when a preceding runner raised RunCmdFailed — on a clean iteration
    all remaining runners are skipped.  Fix runners are also skipped on the
    final iteration even after a failure, so the stage bails without retrying.

    Termination: after an iteration where no RunCmdFailed was raised, if
    HEAD is unchanged (head-stable) the loop exits cleanly.  If HEAD
    advanced, the loop continues (or exits on the final iteration).
    """

    def __init__(
        self,
        entry: StageEntry,
        *,
        body_runners: list[Callable[[], None]],
        max_iterations: int,
    ) -> None:
        super().__init__(entry)
        self._body_runners = body_runners
        self._max_iterations = max_iterations

    @classmethod
    def from_runners(
        cls,
        runners: list[Callable[[], None]],
        *,
        name: str = "loop",
        max_iterations: int,
    ) -> LoopStage:
        from gremlins.pipeline import StageEntry

        entry = StageEntry(
            name=name, type="loop", client=None, prompt_paths=[], options={}
        )
        return cls(entry, body_runners=runners, max_iterations=max_iterations)

    def run(self, pipe: Any) -> None:  # noqa: ARG002
        exhausted = False
        try:
            for iteration in range(1, self._max_iterations + 1):
                head_before = _git.head_sha(self.state.cwd)
                had_failure = False

                for i, runner in enumerate(self._body_runners):
                    if i > 0 and (not had_failure or iteration == self._max_iterations):
                        continue
                    try:
                        runner()
                    except RunCmdFailed:
                        had_failure = True

                if not had_failure:
                    head_after = _git.head_sha(self.state.cwd)
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
            emit_bail(
                self.state.gr_id,
                "other",
                f"loop exhausted {self._max_iterations} iterations",
                child_key=self.state.child_key,
            )
            raise LoopExhausted(f"loop exhausted {self._max_iterations} iterations")
        except LoopExhausted:
            raise
        except (SystemExit, Exception) as exc:
            if not exhausted and not _bail_already_set(
                self.state.gr_id, self.state.child_key
            ):
                emit_bail(
                    self.state.gr_id,
                    "other",
                    f"loop stage failed: {exc}"[:200],
                    child_key=self.state.child_key,
                )
            raise


def _bail_already_set(gr_id: str | None, child_key: str | None) -> bool:
    from gremlins.state import resolve_state_file

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
