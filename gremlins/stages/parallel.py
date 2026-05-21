"""ParallelStage: fan-out/fan-in execution of a parallel YAML block."""

from __future__ import annotations

import asyncio
import dataclasses
import json
import logging
import math
import pathlib
import secrets
import signal
import sys
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
from gremlins.utils import git, proc

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


def _parse_timeout(stage_obj: Stage, child_key: str) -> float | None:
    if not stage_obj.raw_dict:
        return None
    raw_t = stage_obj.raw_dict.get("timeout_seconds")
    if raw_t is None:
        return None
    try:
        parsed_t = float(raw_t)
    except (TypeError, ValueError) as exc:
        raise ValueError(
            f"parallel child {child_key!r}: 'timeout_seconds' must be a number, "
            f"got {raw_t!r}"
        ) from exc
    # Treat <=0 as unset rather than firing wait_for immediately.
    return parsed_t if parsed_t > 0 else None


def _missing_result_detail(child_key: str, returncode: int | None) -> str:
    if returncode is None:
        return (
            f"parallel child {child_key!r}: subprocess exited with no result file "
            f"(returncode unavailable)"
        )
    if returncode == 0:
        return f"parallel child {child_key!r} exited 0 without writing result"
    if returncode < 0:
        try:
            sig_name = signal.Signals(-returncode).name
        except ValueError:
            sig_name = f"signal {-returncode}"
        return (
            f"parallel child {child_key!r} terminated by {sig_name} with no result file"
        )
    return f"parallel child {child_key!r} exited with returncode {returncode} and no result file"


def _build_child_spec_dict(
    stage_obj: Stage, child_st: State, child_key: str, attempt: str
) -> dict[str, Any]:
    return {
        "stage_dict": stage_obj.raw_dict,
        "client": str(child_st.client),
        "session_dir": str(child_st.session_dir),
        "gremlin_id": child_st.data.gremlin_id,
        "worktree": str(child_st.worktree) if child_st.worktree else None,
        "worktree_parent": (
            str(child_st.worktree_parent) if child_st.worktree_parent else None
        ),
        "pipeline_path": child_st.data.pipeline_path or None,
        "child_key": child_key,
        "attempt": attempt,
        "parent_stage": child_st.parent_stage,
        "repo": child_st.repo,
        "instructions": child_st.instructions,
    }


async def _pump_prefixed(stream: asyncio.StreamReader, attempt: str) -> None:
    # Read in chunks (not readline) so a child emitting a single huge
    # un-newlined blob cannot deadlock by filling the pipe buffer.
    # Re-split on newlines so each line still gets the [attempt] prefix.
    while True:
        chunk = await stream.read(4096)
        if not chunk:
            break
        for line in chunk.decode("utf-8", "replace").splitlines(keepends=True):
            sys.stdout.write(f"[{attempt}] {line}")
        sys.stdout.flush()


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
        # In-process mirror of state.json parallel_worktrees[group_name].
        self._worktree_paths: dict[str, pathlib.Path] = {}
        self._base_head: str = ""
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

    # --- state persistence ---

    def _hydrate_from_state(self) -> None:
        if self._worktree_paths:
            return
        sf = resolve_state_file(self._parent_data.gremlin_id)
        if sf is None or not sf.exists():
            return
        try:
            data: dict[str, Any] = json.loads(sf.read_text(encoding="utf-8"))
            groups: dict[str, Any] = data.get("parallel_worktrees") or {}
            entry: dict[str, Any] = groups.get(self._group_name) or {}
            paths: dict[str, str] = entry.get("paths") or {}
            for k, v in paths.items():
                self._worktree_paths[k] = pathlib.Path(v)
            self._base_head = entry.get("base_head", "") or self._base_head
        except Exception as exc:
            logger.warning(
                "parallel group %r: could not hydrate worktree paths: %s",
                self._group_name,
                exc,
            )

    def _persist_state(self) -> None:
        self._parent_data.patch_parallel_worktrees(
            self._group_name,
            base_head=self._base_head,
            paths={k: str(v) for k, v in self._worktree_paths.items()},
        )

    def _clear_persisted_state(self) -> None:
        self._parent_data.patch_parallel_worktrees(
            self._group_name, base_head=None, paths=None
        )

    # --- worktree lifecycle ---

    async def _fan_out(self) -> None:
        self._set_stage(f"{self._group_name}-fanout")
        self._hydrate_from_state()
        prior = list(self._worktree_paths.values())
        if prior:
            await git.remove_worktrees_async(
                str(self._project_root), [str(p) for p in prior]
            )
        self._worktree_paths.clear()
        self._base_head = ""
        self._clear_persisted_state()

        if not await git.in_git_repo_async(cwd=str(self._project_root)):
            return

        await git.prune_worktrees_async(str(self._project_root))
        self._base_head = await git.head_sha_async(cwd=str(self._project_root))

        try:
            for child_key, child_state, _ in self._child_runners:
                wt_dir = await git.setup_detached_worktree_async(
                    str(self._project_root),
                    "HEAD",
                    worktree_parent=self._worktree_parent,
                )
                wt_path = pathlib.Path(wt_dir)
                self._worktree_paths[child_key] = wt_path
                child_state.worktree = wt_path
        except Exception:
            await git.remove_worktrees_async(
                str(self._project_root), [str(p) for p in self._worktree_paths.values()]
            )
            self._worktree_paths.clear()
            raise
        self._persist_state()

    async def _teardown_worktrees(self) -> None:
        await git.remove_worktrees_async(
            str(self._project_root), [str(p) for p in self._worktree_paths.values()]
        )
        self._worktree_paths.clear()
        self._base_head = ""
        self._clear_persisted_state()

    # --- parallel execution ---

    async def _parallel(self) -> None:
        self._set_stage(self._group_name)
        if not self._child_runners:
            return

        self._hydrate_from_state()
        done = self._parent_data.done_for(self._stage_path)
        active = [(k, s, fn) for k, s, fn in self._child_runners if k not in done]
        if not active:
            return

        for child_key, child_state, _ in active:
            if child_key in self._worktree_paths and child_state.worktree is None:
                child_state.worktree = self._worktree_paths[child_key]

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

    def _write_bail_state(self, b: Bail, child_key: str) -> None:
        sf = resolve_state_file(self._parent_data.gremlin_id)
        if sf is None or not sf.exists():
            return
        try:
            pa: dict[str, Any] = (
                json.loads(sf.read_text(encoding="utf-8")).get("parallel_attempts")
                or {}
            )
            self._parent_data.write_bail_file(
                "other", b.reason, attempt=pa.get(child_key) or ""
            )
        except Exception:
            pass

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
            self._write_bail_state(b, child_key)
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
        self._parent_data.patch_parallel_attempt(child_key, attempt)
        spec_path = child_st.session_dir / f"spec_{attempt}.json"
        spec_path.write_text(
            json.dumps(_build_child_spec_dict(stage_obj, child_st, child_key, attempt)),
            encoding="utf-8",
        )
        timeout_s = _parse_timeout(stage_obj, child_key)
        child_proc, pumps = await self._spawn_with_pumps(spec_path, attempt)
        try:
            await self._wait_proc(child_proc, timeout_s, child_key)
        except asyncio.CancelledError:
            await proc.terminate_with_grace(child_proc)
            for p in pumps:
                p.cancel()
            raise
        finally:
            await asyncio.gather(*pumps, return_exceptions=True)
        result = self._read_result(spec_path, child_proc, child_key)
        try:
            cost = float(result.get("cost_usd") or 0.0)
        except (ValueError, TypeError):
            cost = 0.0
        if cost > 0 and math.isfinite(cost):
            self._parent_data.add_subprocess_cost(cost)
        self._handle_result_status(result, child_key)

    async def _spawn_with_pumps(
        self, spec_path: pathlib.Path, attempt: str
    ) -> tuple[asyncio.subprocess.Process, list[asyncio.Task[None]]]:
        child_proc = await asyncio.create_subprocess_exec(
            sys.executable,
            "-m",
            "gremlins.spawn.child",
            str(spec_path),
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        pump_out = asyncio.create_task(
            _pump_prefixed(child_proc.stdout, attempt)  # type: ignore[arg-type]
        )
        pump_err = asyncio.create_task(
            _pump_prefixed(child_proc.stderr, attempt)  # type: ignore[arg-type]
        )
        return child_proc, [pump_out, pump_err]

    async def _wait_proc(
        self,
        child_proc: asyncio.subprocess.Process,
        timeout_s: float | None,
        child_key: str,
    ) -> None:
        if timeout_s is None:
            await child_proc.wait()
            return
        try:
            await asyncio.wait_for(child_proc.wait(), timeout=timeout_s)
        except TimeoutError:
            await proc.terminate_with_grace(child_proc)
            raise RuntimeError(
                f"parallel child {child_key!r} timed out after {timeout_s}s"
            )

    def _read_result(
        self,
        spec_path: pathlib.Path,
        child_proc: asyncio.subprocess.Process,
        child_key: str,
    ) -> dict[str, Any]:
        result_path = pathlib.Path(str(spec_path) + ".result")
        if not result_path.exists():
            raise RuntimeError(_missing_result_detail(child_key, child_proc.returncode))
        return json.loads(result_path.read_text(encoding="utf-8"))

    def _handle_result_status(self, result: dict[str, Any], child_key: str) -> None:
        status = result.get("status")
        if status in ("done", "needs_fix"):
            self._parent_data.mark_done(self._stage_path, child_key)
        elif status == "bail":
            if self._cancel_on_bail:
                self._cancel_siblings()
            self._write_bail_state(Bail(result.get("detail") or ""), child_key)
        else:
            if self._cancel_on_bail:
                self._cancel_siblings()
            raise RuntimeError(
                f"parallel child {child_key!r} error: {result.get('detail') or ''}"
            )

    # --- fan-in ---

    async def _fan_in(self) -> None:
        self._set_stage(f"{self._group_name}-fanin")
        self._hydrate_from_state()
        try:
            await self._do_fan_in()
        finally:
            await self._teardown_worktrees()

    async def _validate_no_mutations(self) -> None:
        for child_key, _, _ in self._child_runners:
            wt = self._worktree_paths.get(child_key)
            if wt is None or not wt.is_dir():
                continue
            child_head = await git.head_sha_async(cwd=str(wt))
            if child_head and child_head != self._base_head:
                raise NotImplementedError(
                    f"parallel child {child_key!r} mutated its worktree "
                    "(fan-in merge for mutating parallel is not yet implemented)"
                )
            if (await git.status_porcelain_async(cwd=str(wt))).strip():
                raise NotImplementedError(
                    f"parallel child {child_key!r} has uncommitted changes "
                    "(fan-in merge for mutating parallel is not yet implemented)"
                )

    def _collect_bails(self) -> tuple[list[str], dict[str, str]]:
        sf = resolve_state_file(self._parent_data.gremlin_id)
        if sf is None or not sf.exists():
            return [], {}
        try:
            data = json.loads(sf.read_text(encoding="utf-8"))
            parallel_attempts: dict[str, str] = data.get("parallel_attempts") or {}
            bailed: list[str] = []
            first_bail: dict[str, str] = {}
            for key, _, _ in self._child_runners:
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

    async def _do_fan_in(self) -> None:
        await git.prune_worktrees_async(str(self._project_root))
        if await git.in_git_repo_async(cwd=str(self._project_root)) and self._base_head:
            await self._validate_no_mutations()

        bailed, first_bail = self._collect_bails()
        should_bail = (
            bool(bailed)
            if self._bail_policy == "any"
            else bool(bailed) and len(bailed) == len(self._child_runners)
        )

        if should_bail and first_bail:
            self._parent_data.write_bail_file(
                first_bail.get("class") or "other",
                first_bail.get("detail") or "",
                attempt=self._parent_data.attempt,
            )
        self._parent_data.patch(_delete=("parallel_attempts",))
        if not should_bail:
            self._parent_data.clear_done(self._stage_path)
        if should_bail:
            raise RuntimeError(
                f"parallel group {self._group_name!r} bailed "
                f"({len(bailed)} child(ren), policy={self._bail_policy!r})"
            )
