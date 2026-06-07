"""Unified internal pipeline entry point."""

from __future__ import annotations

import argparse
import atexit
import datetime
import json
import logging
import math
import os
import pathlib
import secrets
import shutil
import signal
import types
from collections.abc import Callable, Sequence
from typing import Any

from gremlins import paths
from gremlins.artifacts.resolve import resolve_in_map
from gremlins.clients.client import Client
from gremlins.env_file import load_env_file
from gremlins.errors import die
from gremlins.executor.gremlin import Gremlin
from gremlins.logging_setup import configure_logging
from gremlins.permissions.loader import load_policy
from gremlins.permissions.policy import Policy
from gremlins.pipeline import Pipeline as _PipelineData
from gremlins.pipeline.discovery import resolve_pipeline_path
from gremlins.protocols import StageProtocol
from gremlins.stages.outcome import Bail
from gremlins.utils import proc as _proc
from gremlins.utils.git import (
    has_commits,
    has_dirty_worktree,
    in_git_repo,
    stage_gremlins_overlay,
)
from gremlins.utils.yaml_io import YamlLoadError as _YamlLoadError

logger = logging.getLogger(__name__)


def _get_repo() -> str:
    r = _proc.run(["git", "remote", "get-url", "origin"], timeout=10)
    if r.returncode != 0:
        raise RuntimeError(
            f"cannot read git remote: {r.stderr.strip() or r.stdout.strip()}"
        )
    url = r.stdout.strip().removesuffix(".git")
    # handles https://github.com/owner/repo and git@github.com:owner/repo
    owner_repo = url.split("github.com")[-1].lstrip(":/")
    if "/" not in owner_repo:
        raise RuntimeError(
            f"cannot parse owner/repo from remote URL: {r.stdout.strip()!r}"
        )
    return owner_repo


_HANDLED_SIGS = tuple(
    getattr(signal, name)
    for name in ("SIGINT", "SIGTERM", "SIGHUP", "SIGQUIT")
    if hasattr(signal, name)
)
_atexit_log_fn: Callable[[], None] | None = None


def _apply_policy_to_stages(stages: Sequence[StageProtocol], policy: Policy) -> None:
    for stage in stages:
        if stage.client is not None:
            stage.client.set_policy(policy)
        if stage.body:
            _apply_policy_to_stages(stage.body, policy)


def _load_stage_attempt(gremlin: Gremlin) -> tuple[str, str]:
    if gremlin.state and gremlin.state.data:
        return gremlin.state.data.stage or "", gremlin.state.data.attempt or ""
    return "", ""


def _install_signal_handlers(clients: Sequence[Client], gremlin: Gremlin) -> None:
    global _atexit_log_fn

    def handler(signum: int, _frame: types.FrameType | None) -> None:  # pyright: ignore[reportUnusedParameter]
        stage, attempt = _load_stage_attempt(gremlin)
        logger.warning(
            "received %s at stage=%s attempt=%s",
            signal.Signals(signum).name,
            stage or "(none)",
            attempt or "(none)",
        )
        for h in logging.getLogger().handlers:
            try:
                h.flush()
            except Exception:
                pass
        for c in clients:
            try:
                c.reap_all()
            except Exception:
                pass
        signal.signal(signum, signal.SIG_DFL)
        os.kill(os.getpid(), signum)

    for sig in _HANDLED_SIGS:
        signal.signal(sig, handler)

    def _atexit_log() -> None:
        stage, attempt = _load_stage_attempt(gremlin)
        if not stage:
            return
        logger.warning(
            "exiting via atexit at stage=%s attempt=%s",
            stage,
            attempt or "(none)",
        )
        for h in logging.getLogger().handlers:
            try:
                h.flush()
            except Exception:
                pass

    if _atexit_log_fn is not None:
        atexit.unregister(_atexit_log_fn)
    _atexit_log_fn = _atexit_log
    atexit.register(_atexit_log)


def _parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(add_help=False)
    parser.add_argument("--client", dest="client", default=None)
    parser.add_argument("--resume-from", dest="resume_from", default=None)
    return parser.parse_args(argv)


def _unique_clients(stages: Sequence[StageProtocol]) -> list[Client]:
    seen: set[int] = set()
    result: list[Client] = []
    for stage in stages:
        c = stage.client
        if c is not None and id(c) not in seen:
            seen.add(id(c))
            result.append(c)
        body = getattr(stage, "body", [])
        if body:
            for bc in _unique_clients(body):
                if id(bc) not in seen:
                    seen.add(id(bc))
                    result.append(bc)
    return result


def _read_state_json(gremlin_id: str | None) -> dict[str, Any]:
    sf = paths.state_root() / gremlin_id / "state.json" if gremlin_id else None
    if sf is None or not sf.exists():
        return {}
    try:
        return json.loads(sf.read_text(encoding="utf-8"))
    except json.JSONDecodeError:
        return {}


async def run_pipeline(
    pipeline_path: pathlib.Path,
    *,
    argv: list[str],
    gremlin_id: str | None = None,
    client: Client | None = None,
) -> int:
    """Load pipeline YAML, build Gremlin, run. Sole internal pipeline entry point."""
    configure_logging()
    args = _parse_args(argv)
    resume_from = (
        os.environ.pop("GREMLINS_RESUME_FROM", None) or args.resume_from or None
    )

    os.environ.pop("GREMLINS_PROJECT_ROOT", None)
    _project_root = paths.project_root()
    os.environ["GREMLINS_PROJECT_ROOT"] = str(_project_root)

    if shutil.which("git") is None:
        die("git not found on PATH")

    if not in_git_repo():
        die(
            f"gremlins requires a git repository; {paths.project_root()} is not inside a git worktree"
        )

    state_json = _read_state_json(gremlin_id)
    if gremlin_id:
        artifact_dir = paths.state_root() / gremlin_id / "artifacts"
    else:
        ts = datetime.datetime.now().strftime("%Y%m%d-%H%M%S")
        rand = secrets.token_hex(3)
        artifact_dir = paths.state_root() / "direct" / f"{ts}-{rand}" / "artifacts"
    artifact_dir.mkdir(parents=True, exist_ok=True)
    state_dir = artifact_dir.parent
    _workdir = str(state_json.get("workdir") or "")
    worktree_dir = pathlib.Path(_workdir) if _workdir else None
    project_root = str(state_json.get("project_root") or "")
    stage_inputs: dict[str, Any] = dict(state_json.get("stage_inputs") or {})

    # base_ref_sha and base_ref are bound in registry.json at launch time
    _registry_path = state_dir / "registry.json"
    base_ref_sha = ""
    base_ref = ""
    if _registry_path.exists():
        try:
            _reg = json.loads(_registry_path.read_text(encoding="utf-8"))
            _sha_uri = str(_reg.get("base_sha") or "")
            if _sha_uri.startswith("git://commit/"):
                base_ref_sha = _sha_uri.removeprefix("git://commit/")
            _ref_uri = str(_reg.get("base_ref") or "")
            if _ref_uri.startswith("git://ref/"):
                base_ref = _ref_uri.removeprefix("git://ref/")
        except Exception:
            logger.warning(
                "failed to read base_sha/base_ref from registry.json", exc_info=True
            )

    # PR refs like pull/<N>/head need to be fetched from the remote
    fetch_worktree = base_ref.startswith("pull/") and base_ref.endswith("/head")

    project_dir = pathlib.Path(project_root) if project_root else paths.project_root()
    try:
        _pipeline_preview = _PipelineData.from_yaml(
            resolve_pipeline_path(str(pipeline_path), project_dir)
        )
    except (FileNotFoundError, _YamlLoadError, ValueError) as exc:
        die(str(exc))
    gh = _pipeline_preview.github_integration
    if gh and shutil.which("gh") is None:
        die("gh CLI not found")

    logger.info("artifact: %s", artifact_dir)

    gh_repo = _get_repo() if gh else ""
    try:
        gremlin = Gremlin.initialize_with_runtime(
            gremlin_id=gremlin_id,
            state_dir=state_dir,
            project_dir=pathlib.Path(project_root)
            if project_root
            else paths.project_root(),
            pipeline_ref=str(pipeline_path),
            resume_from=resume_from,
            worktree_dir=worktree_dir,
            project_root=project_root,
            base_ref_sha=base_ref_sha,
            base_ref=base_ref,
            fetch_worktree=fetch_worktree,
            client_label=args.client or "",
            repo=gh_repo,
            stage_inputs=stage_inputs,
            client=client,
        )
        gremlin.validate_resume_target()
    except ValueError as exc:
        die(str(exc))

    stage_gremlins_overlay(str(_project_root), state_dir)

    _env_file = paths.project_overlay_dir(_project_root) / "env"
    if _env_file.is_file():
        os.environ["GREMLINS_WORKTREE_PATH"] = (
            str(gremlin.worktree_dir) if gremlin.worktree_dir else ""
        )
        os.environ["GREMLINS_ARTIFACT_DIR"] = str(gremlin.artifact_dir)
        try:
            os.environ.update(load_env_file(_env_file, cwd=_project_root))
        except RuntimeError as exc:
            die(str(exc))

    stored_bypass = bool(state_json.get("bypass", False))
    policy = load_policy(
        cli_bypass=stored_bypass,
        cli_permissions_file=None,
        env=os.environ,
        cwd=pathlib.Path(project_root) if project_root else paths.project_root(),
    )
    _apply_policy_to_stages(gremlin.stages, policy)

    if gh:
        gremlin.state_file = gremlin.state_dir / "state.json"

    _stage_clients = _unique_clients(gremlin.stages)
    _signal_clients = [client] if client is not None else _stage_clients

    if any(c.provider == "claude" for c in _signal_clients):
        if shutil.which("claude") is None:
            die("claude not found on PATH")

    if not gh and resume_from:
        _expanded_stage_names = [s.name for s in gremlin.stages]

        def _name_idx(stage_name: str) -> int:
            for i, s in enumerate(gremlin.stages):
                if s.name == stage_name:
                    return i
            return len(gremlin.stages)

        start_idx = (
            _expanded_stage_names.index(resume_from)
            if resume_from in _expanded_stage_names
            else 0
        )
        if start_idx >= _name_idx("review-code"):
            if not has_dirty_worktree() and not has_commits():
                die(
                    f"--resume-from {resume_from} requires implementation changes in the worktree"
                )

    _install_signal_handlers(_signal_clients, gremlin)
    try:
        await gremlin.run()
    except Bail as b:
        gremlin.state.data.write_bail_file("other", b.reason, attempt=gremlin.state.data.attempt)
        return 1
    except Exception as exc:
        gremlin.state.data.write_bail_file(
            "other", f"unexpected error: {exc}"[:200], attempt=gremlin.state.data.attempt
        )
        raise

    total_cost = 0.0
    for c in [client] if client else _stage_clients:
        total_cost += getattr(c, "total_cost_usd", 0.0) or 0.0
    try:
        subprocess_cost = float(gremlin.state.data.read_str("subprocess_cost_usd") or 0.0)
    except (ValueError, TypeError):
        subprocess_cost = 0.0
    if math.isfinite(subprocess_cost) and subprocess_cost >= 0:
        total_cost += subprocess_cost
    if total_cost > 0:
        gremlin.state.data.patch(total_cost_usd=total_cost)

    if gh:
        pr_url = resolve_in_map(gremlin.registry, {"pr_url": "pr-url?(unknown)"})[
            "pr_url"
        ]
        logger.info("done. PR: %s", pr_url)
    else:
        logger.info("done. artifacts in: %s", artifact_dir)
    if total_cost > 0:
        logger.info("total cost: $%.4f", total_cost)

    return 0
