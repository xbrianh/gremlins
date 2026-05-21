"""ParallelStage: fan-out/fan-in execution of a parallel YAML block."""

from __future__ import annotations

import asyncio
import dataclasses
import logging
import math
import pathlib
import secrets
from collections.abc import Callable
from typing import Any, cast

from gremlins.executor.parallel_state import ParallelGroupState
from gremlins.executor.state import State, StateData
from gremlins.stages.base import Stage
from gremlins.stages.composite import child_state as _child_state
from gremlins.stages.outcome import Bail, Done, Outcome
from gremlins.utils import git, parallel_bail, proc

logger = logging.getLogger(__name__)

_Stage = tuple[str, Callable[[], Any]]


def _noop_set_stage(_n: str) -> None:
    pass


class ParallelStage(Stage):
    """Fan-out/fan-in execution of a parallel pipeline block."""

    type = "parallel"

    def __init__(
        self,
        name: str,
        body: list[Stage],
        *,
        max_concurrent: int | None = None,
        cancel_on_bail: bool = False,
        bail_policy: str = "any",
    ) -> None:
        super().__init__(name)
        self._max_concurrent = max_concurrent
        self._cancel_on_bail = cancel_on_bail
        self._bail_policy = bail_policy
        self.body = body
        for c in self.body:
            c.path = f"{name}/{c.name}"

    @property
    def max_concurrent(self) -> int | None:
        return self._max_concurrent

    @property
    def cancel_on_bail(self) -> bool:
        return self._cancel_on_bail

    @property
    def bail_policy(self) -> str:
        return self._bail_policy

    @classmethod
    def with_dict(cls, d: dict[str, Any], depth: int = 0) -> ParallelStage:
        from gremlins.pipeline.loader import parse_stages

        if depth > 0:
            raise ValueError(
                f"nested parallel groups are not allowed (stage {d.get('name', '?')!r})"
            )
        name = d.get("name") or ""
        children_field: object = d.get("parallel") or []
        if not isinstance(children_field, list):
            raise ValueError(f"parallel group {name!r}: 'parallel' must be a list")
        body = parse_stages(cast(list[dict[str, Any]], children_field), depth=depth + 1)
        seen: set[str] = set()
        for child in body:
            if child.name in seen:
                raise ValueError(
                    f"parallel group {name!r}: duplicate child name {child.name!r}"
                )
            seen.add(child.name)
        max_concurrent = d.get("max_concurrent")
        if max_concurrent is not None and (
            not isinstance(max_concurrent, int) or max_concurrent <= 0
        ):
            raise ValueError(
                f"parallel group {name!r}: 'max_concurrent' must be a positive integer"
            )
        raw_cancel = d.get("cancel_on_bail", False)
        if not isinstance(raw_cancel, bool):
            raise ValueError(
                f"parallel group {name!r}: 'cancel_on_bail' must be a boolean"
            )
        bail_policy = str(d.get("bail_policy") or "any")
        if bail_policy not in ("any", "all"):
            raise ValueError(
                f"parallel group {name!r}: 'bail_policy' must be 'any' or 'all'"
            )
        return cls(
            name,
            body,
            max_concurrent=max_concurrent,
            cancel_on_bail=raw_cancel,
            bail_policy=bail_policy,
        )

    def build_runtime_stages(
        self,
        child_runners: list[tuple[str, State, Callable[[], Any]]],
        *,
        parent_data: StateData | None = None,
        project_root: pathlib.Path | None = None,
        worktree_parent: pathlib.Path | None = None,
        set_stage_fn: Callable[[str], None] | None = None,
        child_stages: list[Stage] | None = None,
    ) -> list[_Stage]:
        """Return the three runtime stages for this parallel block."""
        return _ParallelExecutor(
            self.name,
            child_runners,
            max_concurrent=self._max_concurrent,
            set_stage_fn=set_stage_fn or _noop_set_stage,
            cancel_on_bail=self._cancel_on_bail,
            bail_policy=self._bail_policy,
            parent_data=parent_data or StateData(),
            project_root=project_root or pathlib.Path.cwd(),
            worktree_parent=worktree_parent,
            stage_path=self.path or self.name,
            child_stages=child_stages,
        ).runtime_stages()

    async def run(self, state: State) -> Outcome:
        group_dir = state.session_dir / self.name
        group_dir.mkdir(parents=True, exist_ok=True)
        group_state = dataclasses.replace(
            state, session_dir=group_dir, parent_stage=state.parent_stage or self.name
        )
        for child in self.body:
            (group_dir / child.name).mkdir(parents=True, exist_ok=True)
        done = state.data.done_for(self.path or self.name)
        child_runners: list[tuple[str, State, Callable[[], Any]]] = []
        for child in self.body:
            if child.name in done:
                continue
            cs = _child_state(group_state, child, fan_out=True)
            runner = cs.make_runner(child, scope=self.body)
            child_runners.append((child.name, cs, runner))
        for _, fn in self.build_runtime_stages(
            child_runners,
            parent_data=state.data,
            project_root=pathlib.Path.cwd(),
            worktree_parent=state.worktree_parent,
            set_stage_fn=lambda n: state.record_stage_progress(self.name, sub_stage=n),
            child_stages=self.body,
        ):
            await fn()
        return Done()


class _ParallelExecutor:
    """Manages fan-out/fan-in execution for one parallel group."""

    def __init__(
        self,
        group_name: str,
        child_runners: list[tuple[str, State, Callable[[], Any]]],
        *,
        max_concurrent: int | None,
        set_stage_fn: Callable[[str], None],
        cancel_on_bail: bool,
        bail_policy: str,
        parent_data: StateData,
        project_root: pathlib.Path,
        worktree_parent: pathlib.Path | None = None,
        stage_path: str = "",
        child_stages: list[Stage] | None = None,
    ) -> None:
        self._group_name = group_name
        self._child_runners = child_runners
        self._set_stage = set_stage_fn
        self._cancel_on_bail = cancel_on_bail
        self._bail_policy = bail_policy
        self._parent_data = parent_data
        self._project_root = project_root
        self._worktree_parent = worktree_parent
        self._stage_path = stage_path
        self._stages_by_key: dict[str, Stage] = (
            {st.name: st for st in child_stages} if child_stages else {}
        )
        self._group_state = ParallelGroupState(group_name, parent_data)
        self._tasks: list[asyncio.Task[None]] = []
        self._sem: asyncio.Semaphore | None = (
            asyncio.Semaphore(max_concurrent) if max_concurrent is not None else None
        )

    def runtime_stages(self) -> list[_Stage]:
        fanout_name = f"{self._group_name}-fanout"
        fanin_name = f"{self._group_name}-fanin"
        return [
            (fanout_name, self._fan_out),
            (self._group_name, self._parallel),
            (fanin_name, self._fan_in),
        ]

    # --- worktree lifecycle ---

    async def _fan_out(self) -> None:
        self._set_stage(f"{self._group_name}-fanout")
        gs = self._group_state
        gs.hydrate()
        prior = list(gs.worktree_paths.values())
        if prior:
            await git.remove_worktrees_async(
                str(self._project_root), [str(p) for p in prior]
            )
        gs.clear()

        if not await git.in_git_repo_async(cwd=str(self._project_root)):
            return

        await git.prune_worktrees_async(str(self._project_root))
        gs.base_head = await git.head_sha_async(cwd=str(self._project_root))

        try:
            for child_key, child_state, _ in self._child_runners:
                wt_dir = await git.setup_detached_worktree_async(
                    str(self._project_root),
                    "HEAD",
                    worktree_parent=self._worktree_parent,
                )
                wt_path = pathlib.Path(wt_dir)
                gs.worktree_paths[child_key] = wt_path
                child_state.worktree = wt_path
        except Exception:
            await git.remove_worktrees_async(
                str(self._project_root), [str(p) for p in gs.worktree_paths.values()]
            )
            gs.worktree_paths.clear()
            raise
        gs.persist()

    async def _teardown_worktrees(self) -> None:
        gs = self._group_state
        await git.remove_worktrees_async(
            str(self._project_root), [str(p) for p in gs.worktree_paths.values()]
        )
        gs.clear()

    # --- parallel execution ---

    async def _parallel(self) -> None:
        self._set_stage(self._group_name)
        if not self._child_runners:
            return

        self._group_state.hydrate()
        done = self._parent_data.done_for(self._stage_path)
        active = [(k, s, fn) for k, s, fn in self._child_runners if k not in done]
        if not active:
            return

        for child_key, child_state, _ in active:
            wt = self._group_state.worktree_paths.get(child_key)
            if wt is not None and child_state.worktree is None:
                child_state.worktree = wt

        # Snapshot of all dispatched keys; not updated per-task as children finish.
        self._parent_data.patch(active_children=[k for k, _, _ in active])
        try:
            self._tasks = [
                asyncio.create_task(self._dispatch(k, s, fn)) for k, s, fn in active
            ]
            results = await asyncio.gather(*self._tasks, return_exceptions=True)
        finally:
            self._parent_data.patch(_delete=("active_children",))
        errors = [r for r in results if isinstance(r, Exception)]
        if errors:
            for extra in errors[1:]:
                logger.error("parallel child also failed: %s", extra)
            raise errors[0]

    def _cancel_siblings(self) -> None:
        for t in self._tasks:
            if not t.done():
                t.cancel()

    async def _dispatch(
        self, child_key: str, child_st: State, fn: Callable[[], Any]
    ) -> None:
        stage_obj = self._stages_by_key.get(child_key)
        use_subprocess = stage_obj is not None and stage_obj.raw_dict is not None

        async def _run() -> None:
            if use_subprocess:
                assert stage_obj is not None
                await self._run_subprocess(child_key, child_st, stage_obj)
            else:
                await self._run_child(child_key, fn)

        if self._sem is not None:
            async with self._sem:
                await _run()
        else:
            await _run()

    async def _run_child(self, child_key: str, fn: Callable[[], Any]) -> None:
        try:
            await fn()
        except asyncio.CancelledError:
            raise
        except Bail as b:
            if self._cancel_on_bail:
                self._cancel_siblings()
            self._group_state.write_bail(child_key, b.reason)
            return
        except Exception:
            if self._cancel_on_bail:
                self._cancel_siblings()
            raise
        self._parent_data.mark_done(self._stage_path, child_key)

    # --- subprocess runner ---

    async def _run_subprocess(
        self, child_key: str, child_st: State, stage_obj: Stage
    ) -> None:
        attempt = f"{child_key}-{secrets.token_hex(4)}"
        self._group_state.record_attempt(child_key, attempt)

        def on_bail(detail: str) -> None:
            if self._cancel_on_bail:
                self._cancel_siblings()
            self._group_state.write_bail(child_key, detail)

        status, cost = await proc.run_child_subprocess(
            stage_obj, child_st, child_key, attempt, on_bail=on_bail
        )
        if cost > 0 and math.isfinite(cost):
            self._parent_data.add_subprocess_cost(cost)
        if status == "done":
            self._parent_data.mark_done(self._stage_path, child_key)

    # --- fan-in ---

    async def _fan_in(self) -> None:
        self._set_stage(f"{self._group_name}-fanin")
        self._group_state.hydrate()
        try:
            await self._do_fan_in()
        finally:
            await self._teardown_worktrees()

    async def _validate_no_mutations(self) -> None:
        gs = self._group_state
        for child_key, _, _ in self._child_runners:
            wt = gs.worktree_paths.get(child_key)
            if wt is None or not wt.is_dir():
                continue
            child_head = await git.head_sha_async(cwd=str(wt))
            if child_head and child_head != gs.base_head:
                raise NotImplementedError(
                    f"parallel child {child_key!r} mutated its worktree "
                    "(fan-in merge for mutating parallel is not yet implemented)"
                )
            if (await git.status_porcelain_async(cwd=str(wt))).strip():
                raise NotImplementedError(
                    f"parallel child {child_key!r} has uncommitted changes "
                    "(fan-in merge for mutating parallel is not yet implemented)"
                )

    async def _do_fan_in(self) -> None:
        await git.prune_worktrees_async(str(self._project_root))
        if (
            await git.in_git_repo_async(cwd=str(self._project_root))
            and self._group_state.base_head
        ):
            await self._validate_no_mutations()

        state_dir, attempts = self._group_state.read_bail_scan_inputs()
        child_keys = [k for k, _, _ in self._child_runners]
        bailed = (
            parallel_bail.collect_bails(state_dir, child_keys, attempts)
            if state_dir is not None
            else []
        )
        decision = parallel_bail.decide(bailed, len(child_keys), self._bail_policy)

        if decision.should_bail and decision.first_bail:
            self._parent_data.write_bail_file(
                decision.first_bail.get("class") or "other",
                decision.first_bail.get("detail") or "",
                attempt=self._parent_data.attempt,
            )
        self._group_state.clear_attempts()
        if not decision.should_bail:
            self._parent_data.clear_done(self._stage_path)
        if decision.should_bail:
            raise RuntimeError(
                f"parallel group {self._group_name!r} bailed "
                f"({len(bailed)} child(ren), policy={self._bail_policy!r})"
            )
