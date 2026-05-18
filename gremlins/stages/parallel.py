"""ParallelStage: fan-out/fan-in execution of a parallel YAML block."""

from __future__ import annotations

import asyncio
import dataclasses
import json
import logging
import pathlib
import secrets
import tempfile
from collections.abc import Callable
from typing import Any, cast

from gremlins.executor.state import (
    State,
    StateData,
    resolve_state_file,
)
from gremlins.stages.base import Stage
from gremlins.stages.composite import child_state as _child_state
from gremlins.stages.outcome import Bail, Done, Outcome
from gremlins.utils import proc

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
        from gremlins.pipeline.loader import parse_stage

        if depth > 0:
            raise ValueError(
                f"nested parallel groups are not allowed (stage {d.get('name', '?')!r})"
            )
        name = d.get("name")
        if not isinstance(name, str) or not name:
            raise ValueError("parallel group must have a 'name' field")
        children_field: object = d.get("parallel") or []
        if not isinstance(children_field, list):
            raise ValueError(f"parallel group {name!r}: 'parallel' must be a list")
        seen: set[str] = set()
        body: list[Stage] = []
        for child_raw in cast(list[dict[str, Any]], children_field):
            child = parse_stage(child_raw, depth=depth + 1)
            if child.name in seen:
                raise ValueError(
                    f"parallel group {name!r}: duplicate child name {child.name!r}"
                )
            seen.add(child.name)
            body.append(child)
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
    ) -> list[_Stage]:
        """Return the three runtime stages for this parallel block."""
        return _parallel_stages(
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
        )

    async def run(self, state: State) -> Outcome:
        group_dir = state.session_dir / self.name
        group_dir.mkdir(parents=True, exist_ok=True)
        group_state = dataclasses.replace(
            state, session_dir=group_dir, parent_stage=state.parent_stage or self.name
        )
        child_runners: list[tuple[str, State, Callable[[], Any]]] = []
        for child in self.body:
            (group_dir / child.name).mkdir(parents=True, exist_ok=True)
            cs = _child_state(group_state, child, fan_out=True)
            runner = cs.make_runner(child, scope=self.body)
            child_runners.append((child.name, cs, runner))
        for _, fn in self.build_runtime_stages(
            child_runners,
            parent_data=state.data,
            project_root=pathlib.Path.cwd(),
            worktree_parent=state.worktree_parent,
            set_stage_fn=lambda n: state.record_stage_progress(self.name, sub_stage=n),
        ):
            await fn()
        return Done()


def _parallel_stages(
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
) -> list[_Stage]:
    gremlin_id = parent_data.gremlin_id
    fanout_name = f"{group_name}-fanout"
    fanin_name = f"{group_name}-fanin"

    # In-process mirror of state.json parallel_worktrees[group_name].
    _worktree_paths: dict[str, pathlib.Path] = {}
    base_head: str = ""

    async def _in_git_repo() -> bool:
        try:
            return await proc.run_ok_async(
                ["git", "rev-parse", "--git-dir"], cwd=str(project_root)
            )
        except Exception:
            return False

    def _hydrate_from_state() -> None:
        nonlocal base_head
        if _worktree_paths:
            return
        sf = resolve_state_file(gremlin_id)
        if sf is None or not sf.exists():
            return
        try:
            data: dict[str, Any] = json.loads(sf.read_text(encoding="utf-8"))
            groups: dict[str, Any] = data.get("parallel_worktrees") or {}
            entry: dict[str, Any] = groups.get(group_name) or {}
            paths: dict[str, str] = entry.get("paths") or {}
            for k, v in paths.items():
                _worktree_paths[k] = pathlib.Path(v)
            base_head = entry.get("base_head", "") or base_head
        except Exception as exc:
            logger.warning(
                "parallel group %r: could not hydrate worktree paths: %s",
                group_name,
                exc,
            )

    def _persist_state() -> None:
        parent_data.patch_parallel_worktrees(
            group_name,
            base_head=base_head,
            paths={k: str(v) for k, v in _worktree_paths.items()},
        )

    def _clear_persisted_state() -> None:
        parent_data.patch_parallel_worktrees(group_name, base_head=None, paths=None)

    async def _remove_worktrees(paths: list[pathlib.Path]) -> None:
        if not await _in_git_repo():
            return
        for wt in paths:
            try:
                await proc.run_quiet_async(
                    ["git", "worktree", "remove", "--force", str(wt)],
                    cwd=str(project_root),
                )
            except Exception:
                pass
        try:
            await proc.run_quiet_async(
                ["git", "worktree", "prune"], cwd=str(project_root)
            )
        except Exception:
            pass

    async def _fan_out() -> None:
        nonlocal base_head
        set_stage_fn(fanout_name)

        _hydrate_from_state()
        prior = list(_worktree_paths.values())
        if prior:
            await _remove_worktrees(prior)
        _worktree_paths.clear()
        base_head = ""
        _clear_persisted_state()
        parent_data.clear_done(stage_path)

        if not await _in_git_repo():
            return

        await proc.run_quiet_async(["git", "worktree", "prune"], cwd=str(project_root))

        r = await proc.run_async(["git", "rev-parse", "HEAD"], cwd=str(project_root))
        base_head = r.stdout.strip() if r.returncode == 0 else ""

        if worktree_parent is not None:
            worktree_parent.mkdir(parents=True, exist_ok=True)

        try:
            for child_key, child_state, _ in child_runners:
                if worktree_parent is not None:
                    wt_dir = str(
                        worktree_parent
                        / f"aibg-parallel-{group_name}-{secrets.token_hex(8)}"
                    )
                else:
                    wt_dir = str(
                        pathlib.Path(tempfile.gettempdir())
                        / f"aibg-parallel-{group_name}-{secrets.token_hex(8)}"
                    )
                r2 = await proc.run_async(
                    ["git", "worktree", "add", "--detach", wt_dir, "HEAD"],
                    cwd=str(project_root),
                )
                if r2.returncode != 0:
                    raise RuntimeError(
                        f"git worktree add failed for parallel child {child_key!r}: "
                        f"{r2.stderr.strip()}"
                    )
                wt_path = pathlib.Path(wt_dir)
                _worktree_paths[child_key] = wt_path
                child_state.worktree = wt_path
        except Exception:
            await _remove_worktrees(list(_worktree_paths.values()))
            _worktree_paths.clear()
            raise
        _persist_state()

    async def _parallel() -> None:
        set_stage_fn(group_name)
        if not child_runners:
            return

        _hydrate_from_state()

        done = parent_data.done_for(stage_path)

        active = [(k, s, fn) for k, s, fn in child_runners if k not in done]
        if not active:
            return

        for child_key, child_state, _ in active:
            if child_key in _worktree_paths and child_state.worktree is None:
                child_state.worktree = _worktree_paths[child_key]

        sem = asyncio.Semaphore(max_concurrent) if max_concurrent is not None else None
        tasks: list[asyncio.Task[None]] = []

        async def _invoke(fn: Callable[[], Any]) -> None:
            await fn()

        def _cancel_siblings() -> None:
            for t in tasks:
                if not t.done():
                    t.cancel()

        def _write_bail_state(b: Bail, child_key: str) -> None:
            sf = resolve_state_file(gremlin_id)
            if sf is None or not sf.exists():
                return
            try:
                pa: dict[str, Any] = (
                    json.loads(sf.read_text(encoding="utf-8")).get("parallel_attempts")
                    or {}
                )
                parent_data.write_bail_file(
                    "other", b.reason, attempt=pa.get(child_key) or ""
                )
            except Exception:
                pass

        async def _run_child(child_key: str, fn: Callable[[], Any]) -> None:
            try:
                if sem is not None:
                    async with sem:
                        await _invoke(fn)
                else:
                    await _invoke(fn)
            except asyncio.CancelledError:
                raise
            except Bail as b:
                if cancel_on_bail:
                    _cancel_siblings()
                _write_bail_state(b, child_key)
                return
            except Exception:
                if cancel_on_bail:
                    _cancel_siblings()
                raise
            parent_data.mark_done(stage_path, child_key)

        tasks = [asyncio.create_task(_run_child(k, fn)) for k, _, fn in active]
        results = await asyncio.gather(*tasks, return_exceptions=True)
        errors = [r for r in results if isinstance(r, Exception)]
        if errors:
            for extra in errors[1:]:
                logger.error("parallel child also failed: %s", extra)
            raise errors[0]

    async def _fan_in() -> None:
        set_stage_fn(fanin_name)
        _hydrate_from_state()
        try:
            await _do_fan_in()
        finally:
            await _teardown_worktrees()

    async def _validate_no_mutations() -> None:
        for child_key, _, _ in child_runners:
            wt = _worktree_paths.get(child_key)
            if wt is None or not wt.is_dir():
                continue
            r = await proc.run_async(["git", "rev-parse", "HEAD"], cwd=str(wt))
            child_head = r.stdout.strip() if r.returncode == 0 else ""
            if child_head and child_head != base_head:
                raise NotImplementedError(
                    f"parallel child {child_key!r} mutated its worktree "
                    "(fan-in merge for mutating parallel is not yet implemented)"
                )
            status_r = await proc.run_async(
                ["git", "status", "--porcelain"], cwd=str(wt)
            )
            if status_r.stdout.strip():
                raise NotImplementedError(
                    f"parallel child {child_key!r} has uncommitted changes "
                    "(fan-in merge for mutating parallel is not yet implemented)"
                )

    def _collect_bails() -> tuple[list[str], dict[str, str]]:
        sf = resolve_state_file(gremlin_id)
        if sf is None or not sf.exists():
            return [], {}
        try:
            data = json.loads(sf.read_text(encoding="utf-8"))
            parallel_attempts: dict[str, str] = data.get("parallel_attempts") or {}
            bailed: list[str] = []
            first_bail: dict[str, str] = {}
            for key, _, _ in child_runners:
                child_attempt = parallel_attempts.get(key) or ""
                if (
                    child_attempt
                    and (sf.parent / f"bail_{child_attempt}.json").exists()
                ):
                    bailed.append(key)
                    if not first_bail:
                        try:
                            first_bail = dict(
                                json.loads(
                                    (
                                        sf.parent / f"bail_{child_attempt}.json"
                                    ).read_text(encoding="utf-8")
                                )
                            )
                        except Exception:
                            first_bail = {"class": "other"}
            return bailed, first_bail
        except RuntimeError:
            raise
        except Exception as exc:
            logger.warning("fan-in bail aggregation failed: %s", exc)
            return [], {}

    async def _do_fan_in() -> None:
        if await _in_git_repo():
            await proc.run_quiet_async(
                ["git", "worktree", "prune"], cwd=str(project_root)
            )
        if await _in_git_repo() and base_head:
            await _validate_no_mutations()

        bailed, first_bail = _collect_bails()

        if bail_policy == "any":
            should_bail = bool(bailed)
        else:  # "all"
            should_bail = bool(bailed) and len(bailed) == len(child_runners)

        if should_bail and first_bail:
            parent_data.write_bail_file(
                first_bail.get("class") or "other",
                first_bail.get("detail") or "",
                attempt=parent_data.attempt,
            )
        parent_data.patch(_delete=("parallel_attempts",))
        if not should_bail:
            parent_data.clear_done(stage_path)
        if should_bail:
            raise RuntimeError(
                f"parallel group {group_name!r} bailed "
                f"({len(bailed)} child(ren), policy={bail_policy!r})"
            )

    async def _teardown_worktrees() -> None:
        nonlocal base_head
        await _remove_worktrees(list(_worktree_paths.values()))
        _worktree_paths.clear()
        base_head = ""
        _clear_persisted_state()

    return [
        (fanout_name, _fan_out),
        (group_name, _parallel),
        (fanin_name, _fan_in),
    ]
