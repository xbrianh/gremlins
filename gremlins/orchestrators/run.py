"""Unified internal pipeline entry point."""

from __future__ import annotations

import argparse
import logging
import os
import pathlib
import shutil

from gremlins.clients.client import PACKAGE_DEFAULT, Client
from gremlins.env_file import load_env_file
from gremlins.errors import die
from gremlins.logging_setup import configure_logging
from gremlins.orchestrators.pipeline import Pipeline
from gremlins.pipeline import Pipeline as _Pipeline
from gremlins.runner import install_signal_handlers
from gremlins.stage_clients import (
    collect_stage_specs,
    load_stage_specs_from_state,
    validate_stage_specs,
)
from gremlins.executor.state import (
    patch_state,
    pipeline_uses_gh,
    read_pr_url,
    resolve_session_dir,
    resolve_state_file,
)
from gremlins.utils.git import has_commits, has_dirty_worktree, in_git_repo
from gremlins.utils.github import get_repo
from gremlins.utils.yaml import YamlLoadError

logger = logging.getLogger(__name__)


def _parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(add_help=False)
    parser.add_argument("--resume-from", dest="resume_from", default=None)
    parser.add_argument("--plan", dest="plan", default=None)
    parser.add_argument("--spec", default=None)
    parser.add_argument("--cmd", dest="cmds", action="append", default=None)
    parser.add_argument(
        "--test-max-attempts", dest="test_max_attempts", type=int, default=3
    )
    parser.add_argument("--client", dest="client", default=None)
    parser.add_argument("instructions", nargs="*")
    args = parser.parse_args(argv)
    if args.resume_from:
        args.instructions = [s for s in args.instructions if s]
    if args.plan and args.instructions:
        die("--plan and positional instructions are mutually exclusive")
    if not args.resume_from and not args.plan and not args.instructions:
        die("one of --plan, --resume-from, or positional instructions is required")
    if args.test_max_attempts <= 0:
        die("--test-max-attempts must be a positive integer")
    if args.cmds is not None:
        for c in args.cmds:
            if not c.strip():
                die("--cmd: command must be a non-empty string")
    return args


def run_pipeline(
    pipeline_path: pathlib.Path,
    *,
    argv: list[str],
    gr_id: str | None = None,
    client: Client | None = None,
) -> int:
    """Load pipeline YAML, build Pipeline, run. Sole internal pipeline entry point."""
    configure_logging()
    args = _parse_args(argv)

    cli_spec: Client | None = None
    if args.client:
        try:
            cli_spec = Client.parse(args.client)
        except ValueError as exc:
            die(str(exc))

    if os.environ.get("GREMLINS_TEST_NOOP_PIPELINE"):
        return 0

    env_file = pathlib.Path(".gremlins/env")
    if env_file.is_file():
        try:
            os.environ.update(load_env_file(env_file))
        except RuntimeError as exc:
            die(str(exc))

    if shutil.which("claude") is None:
        die("claude CLI not found")

    try:
        pipeline = _Pipeline.from_yaml(pipeline_path)
    except (FileNotFoundError, ValueError, YamlLoadError) as exc:
        die(str(exc))

    is_gh = pipeline_uses_gh(pipeline)

    repo = ""
    state_file = resolve_state_file(gr_id)

    if is_gh:
        if shutil.which("gh") is None:
            die("gh CLI not found")
        repo = get_repo()

    stage_specs: dict[str, Client] = {}
    if args.resume_from and gr_id:
        try:
            stage_specs = load_stage_specs_from_state(gr_id)
        except Exception as exc:
            die(f"--resume-from: corrupt state.json stage_clients: {exc}")
        if not stage_specs:
            die(
                "--resume-from: stage_clients not found in state.json (rerun from scratch?)"
            )
    if not stage_specs:
        stage_specs = collect_stage_specs(pipeline, cli_spec)
        if gr_id:
            patch_state(
                gr_id, stage_clients={k: str(v) for k, v in stage_specs.items()}
            )

    _spec_clients: dict[str, Client] = {}

    def _client_for_spec(spec: Client) -> Client:
        if client is not None:
            return client
        key = str(spec)
        if key not in _spec_clients:
            _spec_clients[key] = spec
        return _spec_clients[key]

    for spec in stage_specs.values():
        _client_for_spec(spec)

    default_spec = cli_spec or pipeline.default_client or PACKAGE_DEFAULT
    if client is None:
        _client_for_spec(default_spec)

    session_dir = resolve_session_dir(gr_id)

    if client is not None:
        _signal_clients = [client]
    elif _spec_clients:
        _signal_clients = list(_spec_clients.values())
    else:
        _signal_clients = [default_spec]

    try:
        validate_stage_specs(stage_specs, pipeline)
    except ValueError as exc:
        die(str(exc))

    plan_file = session_dir / "plan.md"

    # For local pipelines, pre-copy the plan file so stages that resume past
    # the plan stage can find plan.md. For gh pipelines, the plan stage itself
    # handles plan_source (may be a file path or an issue ref).
    if not is_gh and args.plan and not plan_file.exists():
        src = pathlib.Path(args.plan)
        if src.is_file():
            shutil.copyfile(src, plan_file)

    logger.info("session: %s", session_dir)

    try:
        pipe = Pipeline(
            pipeline.stages,
            args=args,
            session_dir=session_dir,
            gr_id=gr_id,
            pipeline_data=pipeline,
            repo=repo,
            state_file=state_file if is_gh else None,
            spec_clients=_spec_clients,
            stage_specs=stage_specs,
            test_client=client,
        )
        pipe.validate_resume_target()
    except ValueError as exc:
        die(str(exc))

    if not is_gh and args.resume_from:
        _expanded_stage_names = [s.name for s in pipe.stages]

        def _type_idx(stage_type: str) -> int:
            for i, s in enumerate(pipe.stages):
                if s.type == stage_type:
                    return i
            return len(pipe.stages)

        start_idx = (
            _expanded_stage_names.index(args.resume_from)
            if args.resume_from in _expanded_stage_names
            else 0
        )
        if start_idx >= _type_idx("implement"):
            if not plan_file.exists() or plan_file.stat().st_size == 0:
                die(f"--resume-from {args.resume_from} requires existing {plan_file}")
        if start_idx >= _type_idx("review-code"):
            is_git = in_git_repo()
            if is_git:
                if not has_dirty_worktree() and not has_commits():
                    die(
                        f"--resume-from {args.resume_from} requires implementation changes in the worktree"
                    )
            else:
                has_files = False
                for dirpath, dirnames, filenames in os.walk("."):
                    dirnames[:] = [d for d in dirnames if d != ".git"]
                    try:
                        sd_res = session_dir.resolve()
                        if pathlib.Path(dirpath).resolve() == sd_res:
                            dirnames[:] = []
                            continue
                    except Exception:
                        pass
                    if filenames:
                        has_files = True
                        break
                if not has_files:
                    die(
                        f"--resume-from {args.resume_from} requires implementation changes in the worktree"
                    )

    install_signal_handlers(*_signal_clients)
    pipe.run()

    total_cost = 0.0
    for c in _spec_clients.values() if _spec_clients else [client] if client else []:
        total_cost += getattr(c, "total_cost_usd", 0.0) or 0.0
    if total_cost > 0:
        patch_state(gr_id, total_cost_usd=total_cost)

    if is_gh:
        logger.info("done. PR: %s", read_pr_url(gr_id) or "(unknown)")
    else:
        logger.info("done. session artifacts in: %s", session_dir)
    if total_cost > 0:
        logger.info("total cost: $%.4f", total_cost)

    return 0
