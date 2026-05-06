"""Generic stage runner: signal handler installation and stage sequencing.

``run_stages`` executes a list of ``(name, callable)`` pairs in order,
skipping stages before ``resume_from``. Stage callables raise on failure;
the runner does not catch — propagation is the orchestrator's signal that
a stage bailed.

``install_signal_handlers`` wires SIGINT/SIGTERM to ``client.reap_all()``
followed by ``sys.exit(130)`` so a Ctrl-C'd run doesn't leave orphaned
``claude -p`` processes burning tokens (the parity contract for the bash
``trap 'kill -- -$$'`` shape).

``build_parallel_stages`` materialises a parallel YAML block into three
runtime stages: ``<group>-fanout``, ``<group>``, and ``<group>-fanin``.
Fan-out creates per-child artifact dirs and git worktrees; the parallel
stage runs children concurrently with per-child bail shards; fan-in
aggregates bails, enforces bail_policy, and tears down worktrees.
"""

from __future__ import annotations

import concurrent.futures
import json
import logging
import pathlib
import secrets
import signal
import subprocess
import sys
import tempfile
import threading
import types
from collections.abc import Callable, Sequence
from typing import Any

from gremlins.clients.protocol import ClaudeClient
from gremlins.stages.base import StageContext

logger = logging.getLogger(__name__)

Stage = tuple[str, Callable[[], None]]


def install_signal_handlers(*clients: ClaudeClient) -> None:
    """Register SIGINT/SIGTERM handlers that reap claude children before
    exit. Pass the live ClaudeClient(s) (real or fake) — their ``reap_all`` is
    what gets called."""

    def handler(signum: int, frame: types.FrameType | None) -> None:
        for c in clients:
            try:
                c.reap_all()
            except Exception:
                pass
        sys.exit(130)

    signal.signal(signal.SIGINT, handler)
    signal.signal(signal.SIGTERM, handler)


def build_parallel_stages(
    group_name: str,
    child_runners: list[tuple[str, StageContext, Callable[[], None]]],
    *,
    max_concurrent: int | None,
    set_stage_fn: Callable[[str], None],
    cancel_on_bail: bool,
    bail_policy: str,
    gr_id: str | None,
    project_root: pathlib.Path,
) -> list[Stage]:
    """Return three stages for a parallel block: fanout, parallel, fanin.

    ``child_runners`` is a list of ``(child_key, ctx, fn)`` triples. Fan-out
    populates ``ctx.worktree`` for each child before the parallel stage runs.

    Resume targets: ``<group>-fanout`` re-runs fan-out through fan-in;
    ``<group>`` re-runs only the parallel and fan-in stages (worktrees must
    already exist from a prior fan-out or will be skipped if not present);
    ``<group>-fanin`` re-aggregates whatever shards exist without rerunning
    workers. Child names are not valid resume targets — resuming a parallel
    block always restarts at one of the three group-level stages.
    """
    fanout_name = f"{group_name}-fanout"
    fanin_name = f"{group_name}-fanin"

    # In-process mirror of the worktree paths and base HEAD persisted to
    # state.json under parallel_worktrees[group_name]. Hydrated from
    # state.json when stages run in a fresh process (resume).
    _worktree_paths: dict[str, pathlib.Path] = {}
    _base_head: list[str] = [""]  # list so nested functions can mutate

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
        """Populate _worktree_paths/_base_head from state.json, if present."""
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

        # Tear down any worktrees the previous run recorded for this group
        # before creating fresh ones, so resumed runs don't leak temp dirs.
        _hydrate_from_state()
        prior = list(_worktree_paths.values())
        if prior:
            _remove_worktrees(prior)
        _worktree_paths.clear()
        _base_head[0] = ""
        _clear_persisted_state()

        if not _in_git_repo():
            return

        # Prune stale worktree refs from prior interrupted runs.
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

        # Hydrate worktree paths from state.json if this is a fresh-process
        # resume directly into the parallel stage (fan-out skipped).
        _hydrate_from_state()

        # Set worktree on ctx for any children that fan-out already populated.
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
        # Hydrate worktree paths from state.json if this is a fresh-process
        # resume directly into fan-in (fan-out and parallel skipped).
        _hydrate_from_state()
        try:
            _do_fan_in()
        finally:
            _teardown_worktrees()

    def _do_fan_in() -> None:
        from gremlins.state import emit_bail, patch_state, resolve_state_file

        # Defensive prune for any leftovers before we start.
        if _in_git_repo():
            subprocess.run(
                ["git", "worktree", "prune"],
                cwd=str(project_root),
                capture_output=True,
                check=False,
            )

        # Mutation check: fan-in for mutating parallel is not yet implemented.
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

        # Aggregate per-child bail shards and apply bail_policy.
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


def run_stages(stages: Sequence[Stage], *, resume_from: str | None = None) -> None:
    """Run stages in order. If ``resume_from`` names one of the stages, all
    stages strictly before it are skipped. Stops at the first exception
    (which the caller is expected to let propagate or handle)."""
    names = [name for name, _ in stages]
    start_idx = 0
    if resume_from is not None:
        if resume_from not in names:
            raise ValueError(f"unknown resume stage {resume_from!r}; valid: {names}")
        start_idx = names.index(resume_from)
    for _name, fn in list(stages)[start_idx:]:
        fn()
