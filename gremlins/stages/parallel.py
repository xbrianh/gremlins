"""ParallelStage: fan-out/fan-in execution of a parallel YAML block."""

from __future__ import annotations

import concurrent.futures
import dataclasses
import json
import logging
import pathlib
import secrets
import tempfile
import threading
from collections.abc import Callable
from typing import Any, cast

from gremlins.executor.state import (
    State,
    StateData,
    resolve_state_file,
)
from gremlins.stages.base import Stage
from gremlins.stages.outcome import Bail, Done, Outcome
from gremlins.utils import proc

logger = logging.getLogger(__name__)

_Stage = tuple[str, Callable[[], None]]


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
        super().__init__(name, None, [], {})
        self._max_concurrent = max_concurrent
        self._cancel_on_bail = cancel_on_bail
        self._bail_policy = bail_policy
        self.body = body

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
        child_runners: list[tuple[str, State, Callable[[], None]]],
        *,
        gremlin_id: str | None = None,
        project_root: pathlib.Path | None = None,
        worktree_parent: pathlib.Path | None = None,
        set_stage_fn: Callable[[str], None] | None = None,
        parent_attempt: str = "",
    ) -> list[_Stage]:
        """Return the three runtime stages for this parallel block."""
        return _parallel_stages(
            self.name,
            child_runners,
            max_concurrent=self._max_concurrent,
            set_stage_fn=set_stage_fn or _noop_set_stage,
            cancel_on_bail=self._cancel_on_bail,
            bail_policy=self._bail_policy,
            gremlin_id=gremlin_id,
            project_root=project_root or pathlib.Path.cwd(),
            worktree_parent=worktree_parent,
            parent_attempt=parent_attempt,
        )

    def run(self, state: State) -> Outcome:
        gremlin_id = state.data.gremlin_id
        group_dir = state.session_dir / self.name
        group_dir.mkdir(parents=True, exist_ok=True)
        child_runners: list[tuple[str, State, Callable[[], None]]] = []
        for child in self.body:
            child_dir = group_dir / child.name
            child_dir.mkdir(parents=True, exist_ok=True)
            child_state = dataclasses.replace(
                state,
                client=state.test_client or child.client,
                session_dir=child_dir,
                child_key=child.name,
                parent_stage=state.parent_stage or self.name,
            )
            child_runners.append(
                (
                    child.name,
                    child_state,
                    child_state.make_runner(child, scope=self.body),
                )
            )
        for _, fn in self.build_runtime_stages(
            child_runners,
            gremlin_id=gremlin_id,
            project_root=pathlib.Path.cwd(),
            worktree_parent=state.worktree_parent,
            set_stage_fn=lambda n: StateData.load(gremlin_id).set_stage(
                self.name, sub_stage=n
            ),
            parent_attempt=state.data.attempt,
        ):
            fn()
        return Done()


def _parallel_stages(
    group_name: str,
    child_runners: list[tuple[str, State, Callable[[], None]]],
    *,
    max_concurrent: int | None,
    set_stage_fn: Callable[[str], None],
    cancel_on_bail: bool,
    bail_policy: str,
    gremlin_id: str | None,
    project_root: pathlib.Path,
    worktree_parent: pathlib.Path | None = None,
    parent_attempt: str = "",
) -> list[_Stage]:
    fanout_name = f"{group_name}-fanout"
    fanin_name = f"{group_name}-fanin"

    # In-process mirror of state.json parallel_worktrees[group_name].
    _worktree_paths: dict[str, pathlib.Path] = {}
    base_head: str = ""

    def _in_git_repo() -> bool:
        try:
            return proc.run_ok(["git", "rev-parse", "--git-dir"], cwd=str(project_root))
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
        StateData.load(gremlin_id).patch_parallel_worktrees(
            group_name,
            base_head=base_head,
            paths={k: str(v) for k, v in _worktree_paths.items()},
        )

    def _clear_persisted_state() -> None:
        StateData.load(gremlin_id).patch_parallel_worktrees(
            group_name, base_head=None, paths=None
        )

    def _remove_worktrees(paths: list[pathlib.Path]) -> None:
        if not _in_git_repo():
            return
        for wt in paths:
            try:
                proc.run_quiet(
                    ["git", "worktree", "remove", "--force", str(wt)],
                    cwd=str(project_root),
                )
            except Exception:
                pass
        try:
            proc.run_quiet(["git", "worktree", "prune"], cwd=str(project_root))
        except Exception:
            pass

    def _fan_out() -> None:
        nonlocal base_head
        set_stage_fn(fanout_name)

        _hydrate_from_state()
        prior = list(_worktree_paths.values())
        if prior:
            _remove_worktrees(prior)
        _worktree_paths.clear()
        base_head = ""
        _clear_persisted_state()
        StateData.load(gremlin_id).patch_parallel_done(group_name)

        if not _in_git_repo():
            return

        proc.run_quiet(["git", "worktree", "prune"], cwd=str(project_root))

        r = proc.run(["git", "rev-parse", "HEAD"], cwd=str(project_root))
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
                r2 = proc.run(
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
            _remove_worktrees(list(_worktree_paths.values()))
            _worktree_paths.clear()
            raise
        _persist_state()

    def _parallel() -> None:
        set_stage_fn(group_name)
        if not child_runners:
            return

        _hydrate_from_state()

        done: set[str] = set()
        sf = resolve_state_file(gremlin_id)
        if sf is not None and sf.exists():
            try:
                raw = json.loads(sf.read_text(encoding="utf-8"))
                done_map: dict[str, Any] = raw.get("parallel_done") or {}
                done_group: dict[str, Any] = done_map.get(group_name) or {}
                done = set(done_group.keys())
            except Exception:
                pass

        active = [(k, s, fn) for k, s, fn in child_runners if k not in done]
        if not active:
            return

        for child_key, child_state, _ in active:
            if child_key in _worktree_paths and child_state.worktree is None:
                child_state.worktree = _worktree_paths[child_key]

        workers = max_concurrent if max_concurrent is not None else len(active)
        cancel_event = threading.Event() if cancel_on_bail else None

        def _run_child(child_key: str, fn: Callable[[], None]) -> None:
            if cancel_event is not None and cancel_event.is_set():
                return
            try:
                fn()
            except Bail as b:
                if cancel_event is not None:
                    cancel_event.set()
                sf = resolve_state_file(gremlin_id)
                if sf is not None and sf.exists():
                    try:
                        pa: dict[str, Any] = json.loads(sf.read_text(encoding="utf-8")).get("parallel_attempts") or {}
                        StateData.load(gremlin_id).write_bail_file("other", b.reason, attempt=pa.get(child_key) or "")
                    except Exception:
                        pass
                return
            except Exception:
                if cancel_event is not None:
                    cancel_event.set()
                raise
            StateData.load(gremlin_id).patch_parallel_done(group_name, child_key)

        with concurrent.futures.ThreadPoolExecutor(max_workers=workers) as pool:
            futs = [pool.submit(_run_child, k, fn) for k, _, fn in active]

        errors = [e for fut in futs if (e := fut.exception()) is not None]
        if errors:
            for extra in errors[1:]:
                logger.error("parallel child also failed: %s", extra)
            raise errors[0]

    def _fan_in() -> None:
        set_stage_fn(fanin_name)
        _hydrate_from_state()
        try:
            _do_fan_in()
        finally:
            _teardown_worktrees()

    def _do_fan_in() -> None:
        if _in_git_repo():
            proc.run_quiet(["git", "worktree", "prune"], cwd=str(project_root))

        base = base_head
        if _in_git_repo() and base:
            for child_key, _, _ in child_runners:
                wt = _worktree_paths.get(child_key)
                if wt is None or not wt.is_dir():
                    continue
                r = proc.run(["git", "rev-parse", "HEAD"], cwd=str(wt))
                child_head = r.stdout.strip() if r.returncode == 0 else ""
                if child_head and child_head != base:
                    raise NotImplementedError(
                        f"parallel child {child_key!r} mutated its worktree "
                        "(fan-in merge for mutating parallel is not yet implemented)"
                    )
                status_r = proc.run(["git", "status", "--porcelain"], cwd=str(wt))
                if status_r.stdout.strip():
                    raise NotImplementedError(
                        f"parallel child {child_key!r} has uncommitted changes "
                        "(fan-in merge for mutating parallel is not yet implemented)"
                    )

        sf = resolve_state_file(gremlin_id)
        bailed: list[str] = []
        should_bail = False
        if sf is not None and sf.exists():
            try:
                data = json.loads(sf.read_text(encoding="utf-8"))
                parallel_attempts: dict[str, str] = data.get("parallel_attempts") or {}
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

                if bail_policy == "any":
                    should_bail = bool(bailed)
                elif bail_policy == "all":
                    should_bail = bool(bailed) and len(bailed) == len(child_runners)

                if should_bail and first_bail:
                    StateData.load(gremlin_id).write_bail_file(
                        first_bail.get("class") or "other",
                        first_bail.get("detail") or "",
                        attempt=parent_attempt,
                    )

                StateData.load(gremlin_id).patch(_delete=("parallel_attempts",))
                if not should_bail:
                    StateData.load(gremlin_id).patch_parallel_done(group_name)
            except RuntimeError:
                raise
            except Exception as exc:
                logger.warning("fan-in bail aggregation failed: %s", exc)

        if should_bail:
            raise RuntimeError(
                f"parallel group {group_name!r} bailed "
                f"({len(bailed)} child(ren), policy={bail_policy!r})"
            )

    def _teardown_worktrees() -> None:
        nonlocal base_head
        _remove_worktrees(list(_worktree_paths.values()))
        _worktree_paths.clear()
        base_head = ""
        _clear_persisted_state()

    return [
        (fanout_name, _fan_out),
        (group_name, _parallel),
        (fanin_name, _fan_in),
    ]
