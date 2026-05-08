"""ParallelStage: fan-out/fan-in execution of a parallel YAML block."""

from __future__ import annotations

import concurrent.futures
import json
import logging
import pathlib
import secrets
import subprocess
import tempfile
import threading
from collections.abc import Callable
from typing import TYPE_CHECKING, Any

from gremlins.stages.base import StageContext
from gremlins.stages.compound import CompoundStage

if TYPE_CHECKING:
    from gremlins.pipeline import StageEntry

logger = logging.getLogger(__name__)

_Stage = tuple[str, Callable[[], None]]


class ParallelStage(CompoundStage):
    """Fan-out/fan-in execution of a parallel pipeline block.

    Call ``build_runtime_stages()`` to get the three ``(name, fn)`` pairs
    that ``_collect_stages`` extends into the run list.  The three stages are
    ``<name>-fanout``, ``<name>``, and ``<name>-fanin``.
    """

    def __init__(
        self,
        entry: StageEntry,
        model: str | None,
        child_runners: list[tuple[str, StageContext, Callable[[], None]]],
        *,
        max_concurrent: int | None,
        cancel_on_bail: bool,
        bail_policy: str,
        gr_id: str | None,
        project_root: pathlib.Path,
        set_stage_fn: Callable[[str], None],
    ) -> None:
        super().__init__(entry, model)
        self._child_runners = child_runners
        self._max_concurrent = max_concurrent
        self._cancel_on_bail = cancel_on_bail
        self._bail_policy = bail_policy
        self._gr_id = gr_id
        self._project_root = project_root
        self._set_stage_fn = set_stage_fn

    def build_runtime_stages(self) -> list[_Stage]:
        """Return the three runtime stages for this parallel block."""
        return _parallel_stages(
            self.name,
            self._child_runners,
            max_concurrent=self._max_concurrent,
            set_stage_fn=self._set_stage_fn,
            cancel_on_bail=self._cancel_on_bail,
            bail_policy=self._bail_policy,
            gr_id=self._gr_id,
            project_root=self._project_root,
        )

    def run(self, pipe: Any) -> None:
        raise NotImplementedError("use build_runtime_stages()")


def _parallel_stages(
    group_name: str,
    child_runners: list[tuple[str, StageContext, Callable[[], None]]],
    *,
    max_concurrent: int | None,
    set_stage_fn: Callable[[str], None],
    cancel_on_bail: bool,
    bail_policy: str,
    gr_id: str | None,
    project_root: pathlib.Path,
) -> list[_Stage]:
    fanout_name = f"{group_name}-fanout"
    fanin_name = f"{group_name}-fanin"

    # In-process mirror of state.json parallel_worktrees[group_name].
    _worktree_paths: dict[str, pathlib.Path] = {}
    _base_head: list[str] = [""]  # list so nested closures can mutate

    def _in_git_repo() -> bool:
        try:
            r = subprocess.run(
                ["git", "rev-parse", "--git-dir"],
                cwd=str(project_root),
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                check=False,
            )
            return r.returncode == 0
        except Exception:
            return False

    def _hydrate_from_state() -> None:
        from gremlins.state import resolve_state_file

        if _worktree_paths:
            return
        sf = resolve_state_file(gr_id)
        if sf is None or not sf.exists():
            return
        try:
            data: dict[str, Any] = json.loads(sf.read_text(encoding="utf-8"))
            groups: dict[str, Any] = data.get("parallel_worktrees") or {}
            entry: dict[str, Any] = groups.get(group_name) or {}
            paths: dict[str, str] = entry.get("paths") or {}
            for k, v in paths.items():
                _worktree_paths[k] = pathlib.Path(v)
            _base_head[0] = entry.get("base_head", "") or _base_head[0]
        except Exception as exc:
            logger.warning(
                "parallel group %r: could not hydrate worktree paths: %s",
                group_name,
                exc,
            )

    def _persist_state() -> None:
        from gremlins.state import patch_parallel_worktrees

        patch_parallel_worktrees(
            gr_id,
            group_name,
            base_head=_base_head[0],
            paths={k: str(v) for k, v in _worktree_paths.items()},
        )

    def _clear_persisted_state() -> None:
        from gremlins.state import patch_parallel_worktrees

        patch_parallel_worktrees(gr_id, group_name, base_head=None, paths=None)

    def _remove_worktrees(paths: list[pathlib.Path]) -> None:
        if not _in_git_repo():
            return
        for wt in paths:
            try:
                subprocess.run(
                    ["git", "worktree", "remove", "--force", str(wt)],
                    cwd=str(project_root),
                    capture_output=True,
                    check=False,
                )
            except Exception:
                pass
        try:
            subprocess.run(
                ["git", "worktree", "prune"],
                cwd=str(project_root),
                capture_output=True,
                check=False,
            )
        except Exception:
            pass

    def _fan_out() -> None:
        set_stage_fn(fanout_name)

        _hydrate_from_state()
        prior = list(_worktree_paths.values())
        if prior:
            _remove_worktrees(prior)
        _worktree_paths.clear()
        _base_head[0] = ""
        _clear_persisted_state()

        if not _in_git_repo():
            return

        subprocess.run(
            ["git", "worktree", "prune"],
            cwd=str(project_root),
            capture_output=True,
            check=False,
        )

        r = subprocess.run(
            ["git", "rev-parse", "HEAD"],
            cwd=str(project_root),
            capture_output=True,
            text=True,
            check=False,
        )
        _base_head[0] = r.stdout.strip() if r.returncode == 0 else ""

        for child_key, ctx, _ in child_runners:
            wt_dir = str(
                pathlib.Path(tempfile.gettempdir())
                / f"aibg-parallel-{group_name}-{secrets.token_hex(8)}"
            )
            r2 = subprocess.run(
                ["git", "worktree", "add", "--detach", wt_dir, "HEAD"],
                cwd=str(project_root),
                capture_output=True,
                text=True,
                check=False,
            )
            if r2.returncode != 0:
                raise RuntimeError(
                    f"git worktree add failed for parallel child {child_key!r}: "
                    f"{r2.stderr.strip()}"
                )
            wt_path = pathlib.Path(wt_dir)
            _worktree_paths[child_key] = wt_path
            ctx.worktree = wt_path
            _persist_state()

    def _parallel() -> None:
        set_stage_fn(group_name)
        active = child_runners
        if not active:
            return

        _hydrate_from_state()

        for child_key, ctx, _ in active:
            if child_key in _worktree_paths and ctx.worktree is None:
                ctx.worktree = _worktree_paths[child_key]

        workers = max_concurrent if max_concurrent is not None else len(active)
        cancel_event = threading.Event() if cancel_on_bail else None

        def _run_child(fn: Callable[[], None]) -> None:
            if cancel_event is not None and cancel_event.is_set():
                return
            try:
                fn()
            except Exception:
                if cancel_event is not None:
                    cancel_event.set()
                raise

        with concurrent.futures.ThreadPoolExecutor(max_workers=workers) as pool:
            futs = [pool.submit(_run_child, fn) for _, _, fn in active]

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
        from gremlins.state import emit_bail, patch_state, resolve_state_file

        if _in_git_repo():
            subprocess.run(
                ["git", "worktree", "prune"],
                cwd=str(project_root),
                capture_output=True,
                check=False,
            )

        base = _base_head[0]
        if _in_git_repo() and base:
            for child_key, _, _ in child_runners:
                wt = _worktree_paths.get(child_key)
                if wt is None or not wt.is_dir():
                    continue
                r = subprocess.run(
                    ["git", "rev-parse", "HEAD"],
                    cwd=str(wt),
                    capture_output=True,
                    text=True,
                    check=False,
                )
                child_head = r.stdout.strip() if r.returncode == 0 else ""
                if child_head and child_head != base:
                    raise NotImplementedError(
                        f"parallel child {child_key!r} mutated its worktree "
                        "(fan-in merge for mutating parallel is not yet implemented)"
                    )
                status_r = subprocess.run(
                    ["git", "status", "--porcelain"],
                    cwd=str(wt),
                    capture_output=True,
                    text=True,
                    check=False,
                )
                if status_r.stdout.strip():
                    raise NotImplementedError(
                        f"parallel child {child_key!r} has uncommitted changes "
                        "(fan-in merge for mutating parallel is not yet implemented)"
                    )

        sf = resolve_state_file(gr_id)
        bailed: list[str] = []
        shards: dict[str, dict[str, str]] = {}
        should_bail = False
        if sf is not None and sf.exists():
            try:
                data = json.loads(sf.read_text(encoding="utf-8"))
                shards = data.get("parallel_bails") or {}
                bailed = [k for k, v in shards.items() if v.get("bail_class")]

                if bail_policy == "any":
                    should_bail = bool(bailed)
                elif bail_policy == "all":
                    should_bail = bool(bailed) and len(bailed) == len(child_runners)

                if should_bail:
                    first_shard = shards[bailed[0]]
                    emit_bail(
                        gr_id,
                        first_shard.get("bail_class", "other"),
                        first_shard.get("bail_detail", ""),
                    )

                patch_state(gr_id, _delete=("parallel_bails",))
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
        _remove_worktrees(list(_worktree_paths.values()))
        _worktree_paths.clear()
        _base_head[0] = ""
        _clear_persisted_state()

    return [
        (fanout_name, _fan_out),
        (group_name, _parallel),
        (fanin_name, _fan_in),
    ]
