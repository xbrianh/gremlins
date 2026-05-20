"""ParallelStage: fan-out/fan-in execution of a parallel YAML block."""

from __future__ import annotations

import asyncio
import dataclasses
import json
import logging
import pathlib
import secrets
import signal
import sys
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
_SIGTERM_GRACE_S = 10.0


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
            child_stages=child_stages,
        )

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
    child_stages: list[Stage] | None = None,
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
        _stages_by_key: dict[str, Stage] = (
            {st.name: st for st in child_stages} if child_stages else {}
        )

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

        async def _run_subprocess(
            child_key: str, child_st: State, stage_obj: Stage
        ) -> None:
            attempt = f"{child_key}-{secrets.token_hex(4)}"
            parent_data.patch_parallel_attempt(child_key, attempt)
            spec_path = child_st.session_dir / f"spec_{attempt}.json"
            spec_path.write_text(
                json.dumps(
                    {
                        "stage_dict": stage_obj.raw_dict,
                        "client": str(child_st.client),
                        "session_dir": str(child_st.session_dir),
                        "gremlin_id": child_st.data.gremlin_id,
                        "worktree": (
                            str(child_st.worktree) if child_st.worktree else None
                        ),
                        "worktree_parent": (
                            str(child_st.worktree_parent)
                            if child_st.worktree_parent
                            else None
                        ),
                        "pipeline_path": child_st.data.pipeline_path or None,
                        "child_key": child_key,
                        "parent_stage": child_st.parent_stage,
                        "repo": child_st.repo,
                        "instructions": child_st.instructions,
                    }
                ),
                encoding="utf-8",
            )
            child_proc = await asyncio.create_subprocess_exec(
                sys.executable,
                "-m",
                "gremlins.spawn.child",
                str(spec_path),
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )

            async def _pump(stream: asyncio.StreamReader) -> None:
                # Read in chunks (not readline) so a child emitting a single huge
                # un-newlined blob cannot deadlock by filling the pipe buffer.
                # Re-split on newlines so each line still gets the [attempt] prefix.
                while True:
                    chunk = await stream.read(4096)
                    if not chunk:
                        break
                    for line in chunk.decode("utf-8", "replace").splitlines(
                        keepends=True
                    ):
                        sys.stdout.write(f"[{attempt}] {line}")
                    sys.stdout.flush()

            pump_out = asyncio.create_task(
                _pump(child_proc.stdout)  # type: ignore[arg-type]
            )
            pump_err = asyncio.create_task(
                _pump(child_proc.stderr)  # type: ignore[arg-type]
            )

            timeout_s: float | None = None
            if stage_obj.raw_dict:
                raw_t = stage_obj.raw_dict.get("timeout_seconds")
                if raw_t is not None:
                    try:
                        parsed_t = float(raw_t)
                    except (TypeError, ValueError) as exc:
                        raise ValueError(
                            f"parallel child {child_key!r}: 'timeout_seconds' must be a "
                            f"number, got {raw_t!r}"
                        ) from exc
                    # Treat <=0 as unset rather than firing wait_for immediately.
                    if parsed_t > 0:
                        timeout_s = parsed_t

            async def _kill_with_grace() -> None:
                # Always escalate to SIGKILL on any exit path (timeout, second
                # cancellation, etc.) so the child is never orphaned.
                try:
                    child_proc.send_signal(signal.SIGTERM)
                except ProcessLookupError:
                    return
                try:
                    await asyncio.shield(
                        asyncio.wait_for(child_proc.wait(), timeout=_SIGTERM_GRACE_S)
                    )
                except (TimeoutError, asyncio.CancelledError):
                    try:
                        child_proc.kill()
                    except ProcessLookupError:
                        pass
                    await asyncio.shield(child_proc.wait())

            try:
                if timeout_s is not None:
                    try:
                        await asyncio.wait_for(child_proc.wait(), timeout=timeout_s)
                    except TimeoutError:
                        await _kill_with_grace()
                        await asyncio.gather(pump_out, pump_err, return_exceptions=True)
                        raise RuntimeError(
                            f"parallel child {child_key!r} timed out after {timeout_s}s"
                        )
                else:
                    await child_proc.wait()
            except asyncio.CancelledError:
                await _kill_with_grace()
                pump_out.cancel()
                pump_err.cancel()
                await asyncio.gather(pump_out, pump_err, return_exceptions=True)
                raise

            await asyncio.gather(pump_out, pump_err, return_exceptions=True)

            result_path = pathlib.Path(str(spec_path) + ".result")
            if not result_path.exists():
                rc = child_proc.returncode
                if rc is None:
                    detail = (
                        f"parallel child {child_key!r}: subprocess exited with no "
                        f"result file (returncode unavailable)"
                    )
                elif rc == 0:
                    detail = (
                        f"parallel child {child_key!r} exited 0 without writing result"
                    )
                elif rc < 0:
                    try:
                        sig_name = signal.Signals(-rc).name
                    except ValueError:
                        sig_name = f"signal {-rc}"
                    detail = (
                        f"parallel child {child_key!r} terminated by {sig_name} "
                        f"with no result file"
                    )
                else:
                    detail = (
                        f"parallel child {child_key!r} exited with returncode {rc} "
                        f"and no result file"
                    )
                raise RuntimeError(detail)

            result: dict[str, Any] = json.loads(result_path.read_text(encoding="utf-8"))
            status = result.get("status")
            if status in ("done", "needs_fix"):
                parent_data.mark_done(stage_path, child_key)
            elif status == "bail":
                if cancel_on_bail:
                    _cancel_siblings()
                _write_bail_state(Bail(result.get("detail") or ""), child_key)
            else:
                if cancel_on_bail:
                    _cancel_siblings()
                raise RuntimeError(
                    f"parallel child {child_key!r} error: {result.get('detail') or ''}"
                )

        async def _run_child(child_key: str, fn: Callable[[], Any]) -> None:
            try:
                if sem is not None:
                    async with sem:
                        await fn()
                else:
                    await fn()
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

        async def _dispatch(
            child_key: str, child_st: State, fn: Callable[[], Any]
        ) -> None:
            stage_obj = _stages_by_key.get(child_key)
            if stage_obj is not None and stage_obj.raw_dict is not None:
                if sem is not None:
                    async with sem:
                        await _run_subprocess(child_key, child_st, stage_obj)
                else:
                    await _run_subprocess(child_key, child_st, stage_obj)
            else:
                await _run_child(child_key, fn)

        parent_data.patch(active_children=[k for k, _, _ in active])
        try:
            tasks = [asyncio.create_task(_dispatch(k, s, fn)) for k, s, fn in active]
            results = await asyncio.gather(*tasks, return_exceptions=True)
        finally:
            parent_data.patch(_delete=("active_children",))
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
