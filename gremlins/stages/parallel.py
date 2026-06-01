"""ParallelStage: fan-out/fan-in execution of a parallel YAML block."""

from __future__ import annotations

import asyncio
import dataclasses
import json
import logging
import math
import pathlib
import re
import secrets
import shutil
from collections.abc import Callable
from typing import TYPE_CHECKING, Any, cast

if TYPE_CHECKING:
    from gremlins.pipeline import Pipeline

from gremlins import paths
from gremlins.artifacts.uri import Uri
from gremlins.clients.client import PACKAGE_DEFAULT
from gremlins.executor.parallel_state import ParallelGroupState
from gremlins.executor.state import State, StateData, build_state
from gremlins.stages.base import Stage
from gremlins.stages.composite import child_state as _child_state
from gremlins.stages.outcome import Bail, Done, Outcome
from gremlins.utils import git, parallel_bail, proc

_CHILD_ID_RE = re.compile(r"^[A-Za-z0-9_-]+$")

logger = logging.getLogger(__name__)

_Stage = tuple[str, Callable[[], Any]]


def _noop_set_stage(_: str) -> None:
    pass


def _branch_pipeline(
    branch_stage: Stage | None, parent_state: State
) -> Pipeline | None:
    from gremlins.pipeline import Pipeline

    if branch_stage is None or branch_stage.raw_dict is None:
        return None
    parent_pipeline = parent_state.pipeline_data
    return Pipeline(
        name=branch_stage.name,
        path=parent_pipeline.path if parent_pipeline else pathlib.Path("."),
        stages=[branch_stage],
        default_client=parent_pipeline.default_client if parent_pipeline else None,
        base_ref=parent_pipeline.base_ref if parent_pipeline else "current",
    )


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
        if name and not _CHILD_ID_RE.match(name):
            raise ValueError(
                f"parallel group name {name!r} contains invalid characters for child_id"
            )
        for child in body:
            if not _CHILD_ID_RE.match(child.name):
                raise ValueError(
                    f"parallel child name {child.name!r} contains invalid characters for child_id"
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
        parent_state: State | None = None,
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
            parent_state=parent_state,
            project_root=project_root or paths.project_root(),
            worktree_parent=worktree_parent,
            stage_path=self.path or self.name,
            child_stages=child_stages,
        ).runtime_stages()

    async def run(self, state: State) -> Outcome:
        parent_id = state.data.gremlin_id or ""
        group_state = dataclasses.replace(
            state, parent_stage=state.parent_stage or self.name
        )
        done = state.data.done_for(self.path or self.name)
        child_runners: list[tuple[str, State, Callable[[], Any]]] = []
        for child in self.body:
            if child.name in done:
                continue
            child_id = f"{parent_id}--{self.name}--{child.name}" if parent_id else ""
            cs = _child_state(
                group_state, child, fan_out=True, child_id=child_id or None
            )
            runner = cs.make_runner(child, scope=self.body)
            child_runners.append((child.name, cs, runner))
        for _, fn in self.build_runtime_stages(
            child_runners,
            parent_data=state.data,
            parent_state=state,
            project_root=paths.project_root(),
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
        parent_state: State | None = None,
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
        self._parent_state = parent_state
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
        from gremlins.executor.gremlin import Gremlin

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

        # base_head must come from the parent's implementation worktree, not the
        # project root.  GREMLINS_PROJECT_ROOT is set to the original repo root
        # before the worktree is created, so paths.project_root() (and therefore
        # self._project_root) always points to the original repo.  fork() creates
        # child worktrees at HEAD(pstate.worktree), which diverges from the
        # original repo HEAD once implement commits.  Using the wrong reference
        # causes _validate_no_mutations to fire a false-positive on every run.
        _pstate_wt = (
            self._parent_state.worktree if self._parent_state is not None else None
        )
        _base_ref = (
            str(_pstate_wt) if _pstate_wt is not None else str(self._project_root)
        )
        gs.base_head = await git.head_sha_async(cwd=_base_ref)
        logger.debug(
            "parallel fan-out: base_head=%s base_ref=%s",
            gs.base_head,
            _base_ref,
        )

        parent_gid = self._parent_data.gremlin_id
        parent_state = self._parent_state
        parent_gremlin: Gremlin | None = None
        if parent_gid:
            parent_gremlin = Gremlin.open(parent_gid)
            if parent_state is not None:
                parent_gremlin.registry = parent_state.artifacts
            else:
                # Resume scenario: reconstruct parent state from disk if not provided
                parent_data = StateData.load(parent_gid)
                parent_state = build_state(
                    data=parent_data,
                    client=parent_gremlin.test_client or PACKAGE_DEFAULT,
                    artifact_dir=parent_gremlin.artifact_dir,
                    worktree_parent=self._worktree_parent,
                )
                parent_gremlin.registry = parent_state.artifacts

        try:
            for child_key, child_state, _ in self._child_runners:
                if (
                    parent_gremlin is not None
                    and parent_gid
                    and parent_state is not None
                    and parent_state.artifact_dir.exists()
                ):
                    child_id = f"{parent_gid}--{self._group_name}--{child_key}"
                    branch_stage = self._stages_by_key.get(child_key)
                    branch_pipeline = _branch_pipeline(branch_stage, parent_state)
                    forked_state = await parent_gremlin.fork(
                        parent_state,
                        child_id,
                        parent_id=parent_gid,
                        group_name=self._group_name,
                        child_key=child_key,
                        pipeline=branch_pipeline,
                    )
                    if forked_state.worktree is not None:
                        child_state.worktree = forked_state.worktree
                        gs.worktree_paths[child_key] = forked_state.worktree
                        logger.debug(
                            "parallel fan-out: forked child=%s worktree=%s",
                            child_key,
                            forked_state.worktree,
                        )
                    else:
                        logger.debug(
                            "parallel fan-out: forked child=%s worktree=None (no worktree created)",
                            child_key,
                        )
                else:
                    if parent_gid and parent_state is None:
                        raise RuntimeError(
                            f"parent gremlin {parent_gid} has no state; cannot fork child {child_key}"
                        )
                    wt_dir = await git.setup_detached_worktree_async(
                        str(self._project_root),
                        gs.base_head or "HEAD",
                        worktree_parent=self._worktree_parent,
                    )
                    wt_path = pathlib.Path(wt_dir)
                    gs.worktree_paths[child_key] = wt_path
                    child_state.worktree = wt_path
                    logger.debug(
                        "parallel fan-out: no-parent child=%s worktree=%s",
                        child_key,
                        wt_path,
                    )
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
        parent_gid = self._parent_data.gremlin_id or ""
        child_id = (
            f"{parent_gid}--{self._group_name}--{child_key}" if parent_gid else ""
        )
        attempt = f"{child_key}-{secrets.token_hex(4)}"
        self._group_state.record_attempt(child_key, attempt)

        def on_bail(detail: str) -> None:
            if self._cancel_on_bail:
                self._cancel_siblings()
            self._group_state.write_bail(child_key, detail)

        status, cost = await proc.run_child_subprocess(
            stage_obj,
            child_st,
            child_key,
            attempt,
            on_bail=on_bail,
            group_name=self._group_name,
            child_id=child_id,
        )
        if cost > 0 and math.isfinite(cost):
            self._parent_data.add_subprocess_cost(cost)
        if status == "done":
            self._parent_data.mark_done(self._stage_path, child_key)

    # --- fan-in ---

    def _rm_child_state_dirs(self) -> None:
        parent_gid = self._parent_data.gremlin_id
        if not parent_gid:
            return
        sr = paths.state_root()
        prefix = f"{parent_gid}--{self._group_name}--"
        for entry in sr.iterdir():
            if entry.name.startswith(prefix) and entry.is_dir():
                shutil.rmtree(entry, ignore_errors=True)

    def _gather_child_artifacts(self) -> None:
        """Copy child artifact bindings into the parent registry before child dirs are removed."""
        parent_state = self._parent_state
        parent_gid = self._parent_data.gremlin_id
        if parent_state is None or not parent_gid:
            return

        sr = paths.state_root()
        parent_keys: set[str] = set(parent_state.artifacts.keys())

        # key -> [(child_key, child_id, uri_str)] — only new bindings not in parent at fan-out
        per_key: dict[str, list[tuple[str, str, str]]] = {}
        for child_key, _, _ in self._child_runners:
            child_id = f"{parent_gid}--{self._group_name}--{child_key}"
            child_reg = sr / child_id / "registry.json"
            if not child_reg.exists():
                continue
            child_bindings: dict[str, str] = json.loads(
                child_reg.read_text(encoding="utf-8")
            )
            for k, v in child_bindings.items():
                if k in parent_keys:
                    continue
                per_key.setdefault(k, []).append((child_key, child_id, v))

        for key, producers in per_key.items():
            multi = len(producers) > 1
            for child_key, child_id, uri_str in producers:
                bound_key = f"{key}/{child_key}" if multi else key
                if uri_str.startswith("file://session/"):
                    name = uri_str[len("file://session/") :]
                    src = sr / child_id / "artifacts" / name
                    if not src.exists():
                        logger.warning("child artifact missing: %s", src)
                        continue
                    dest_name = f"{child_key}/{name}" if multi else name
                    dest = parent_state.artifact_dir / dest_name
                    dest.parent.mkdir(parents=True, exist_ok=True)
                    shutil.copy2(src, dest)
                    parent_state.artifacts.bind(
                        bound_key, Uri.parse(f"file://session/{dest_name}")
                    )
                else:
                    try:
                        parent_state.artifacts.bind(bound_key, Uri.parse(uri_str))
                    except Exception:
                        logger.warning(
                            "failed to bind %s -> %s into parent registry",
                            bound_key,
                            uri_str,
                            exc_info=True,
                        )

    async def _fan_in(self) -> None:
        self._set_stage(f"{self._group_name}-fanin")
        self._group_state.hydrate()
        self._gather_child_artifacts()
        try:
            await self._do_fan_in()
            self._rm_child_state_dirs()
        finally:
            await self._teardown_worktrees()

    async def _validate_no_mutations(self) -> None:
        gs = self._group_state
        for child_key, _, _ in self._child_runners:
            wt = gs.worktree_paths.get(child_key)
            if wt is None or not wt.is_dir():
                continue
            child_head = await git.head_sha_async(cwd=str(wt))
            logger.debug(
                "parallel validate: child=%s worktree=%s child_head=%s base_head=%s",
                child_key,
                wt,
                child_head,
                gs.base_head,
            )
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
