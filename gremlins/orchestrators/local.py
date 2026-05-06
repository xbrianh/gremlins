"""Orchestrator entry points for the local pipeline."""

from __future__ import annotations

import argparse
import dataclasses
import logging
import os
import pathlib
import shutil
import sys
from typing import NoReturn

import yaml

from gremlins.clients import ClientSpec, to_client
from gremlins.clients.protocol import ClaudeClient
from gremlins.clients.resolve import (
    PACKAGE_DEFAULT,
    collect_stage_specs,
    load_stage_specs_from_state,
    require_stage_spec,
    validate_stage_specs,
)
from gremlins.env_file import load_env_file
from gremlins.git import (
    has_commits,
    has_diff,
    has_dirty_worktree,
    in_git_repo,
    rev_exists,
)
from gremlins.logging_setup import configure_logging
from gremlins.orchestrators.pipeline import LocalPipeline
from gremlins.pipeline import (
    load_pipeline,
    resolve_pipeline_path,
)
from gremlins.runner import install_signal_handlers
from gremlins.stages import address_code, review_code
from gremlins.stages.base import StageContext
from gremlins.state import patch_state, resolve_session_dir

logger = logging.getLogger(__name__)


def die(msg: str) -> NoReturn:
    sys.stderr.write(f"error: {msg}\n")
    sys.stderr.flush()
    sys.exit(1)


def _parse_local_args(argv: list[str]) -> argparse.Namespace:
    usage = (
        "usage: gremlins.cli local [--resume-from <stage>] [--plan <path>] [--spec <path>] "
        '[--cmd "<command>"] [--test-max-attempts <n>] '
        "[--pipeline <name-or-path>] [--client <provider:model>] "
        '"<instructions>"'
    )
    parser = argparse.ArgumentParser(add_help=False, usage=usage)
    parser.add_argument("--resume-from", dest="resume_from", default=None)
    parser.add_argument("--plan", dest="plan_path", default=None)
    parser.add_argument("--spec", dest="spec_path", default=None)
    parser.add_argument("--cmd", dest="cmds", action="append", default=None)
    parser.add_argument(
        "--test-max-attempts", dest="test_max_attempts", type=int, default=3
    )
    parser.add_argument("--pipeline", dest="pipeline", default=None)
    parser.add_argument("--client", dest="client", default=None)
    parser.add_argument("instructions", nargs="*")
    args = parser.parse_args(argv)
    if args.resume_from:
        args.instructions = [s for s in args.instructions if s]
    if args.plan_path:
        if args.instructions:
            die("--plan and positional instructions are mutually exclusive")
    else:
        if not args.instructions:
            die(usage)
    if args.test_max_attempts <= 0:
        die("--test-max-attempts must be a positive integer")
    if args.cmds is not None:
        for c in args.cmds:
            if not c.strip():
                die("--cmd: command must be a non-empty string")
    return args



def local_main(
    argv: list[str], *, client: ClaudeClient | None = None, gr_id: str | None = None
) -> int:
    configure_logging()
    args = _parse_local_args(argv)

    cli_spec: ClientSpec | None = None
    if args.client:
        try:
            cli_spec = ClientSpec.parse(args.client)
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
        pipeline = load_pipeline(
            resolve_pipeline_path(args.pipeline or "local", pathlib.Path.cwd())
        )
    except (FileNotFoundError, ValueError, yaml.YAMLError) as exc:
        die(str(exc))

    # Load or resolve stage specs; state.json is authoritative on resume
    stage_specs: dict[str, ClientSpec] = {}
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

    # Create one client instance per unique spec (or reuse injected test client)
    _spec_clients: dict[str, ClaudeClient] = {}

    def _client_for_spec(spec: ClientSpec) -> ClaudeClient:
        if client is not None:
            return client
        key = str(spec)
        if key not in _spec_clients:
            _spec_clients[key] = to_client(spec)
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
        _signal_clients = [to_client(default_spec)]
    try:
        pipe = LocalPipeline(
            pipeline.stages,
            args=args,
            session_dir=session_dir,
            gr_id=gr_id,
            _test_client=client,
            _spec_clients=_spec_clients,
            _stage_specs=stage_specs,
            _pipeline_data=pipeline,
        )
        pipe.validate_resume_target()
    except ValueError as exc:
        die(str(exc))

    _expanded_stage_names = [s.name for s in pipe.stages]

    run_resume_from = args.resume_from

    try:
        validate_stage_specs(stage_specs, pipeline)
    except ValueError as exc:
        die(str(exc))

    plan_file = session_dir / "plan.md"

    # Determine the review-code output file path for the resume guard
    _rc_entry = next((s for s in pipeline.stages if s.type == "review-code"), None)
    if _rc_entry:
        _rc_model = require_stage_spec(stage_specs, _rc_entry.name).model
        review_code_file = session_dir / f"{_rc_entry.name}-{_rc_model}.md"
    else:
        review_code_file = session_dir / f"review-code-{PACKAGE_DEFAULT.model}.md"

    logger.info("session: %s", session_dir)

    is_git = in_git_repo()

    def _type_idx(stage_type: str) -> int:
        # returns an index into pipe.stages, the same index space as _expanded_stage_names
        for i, s in enumerate(pipe.stages):
            if s.type == stage_type:
                return i
        return len(pipe.stages)

    start_idx = 0
    if run_resume_from:
        start_idx = (
            _expanded_stage_names.index(run_resume_from)
            if run_resume_from in _expanded_stage_names
            else 0
        )
        if start_idx >= _type_idx("implement"):
            if not plan_file.exists() or plan_file.stat().st_size == 0:
                die(f"--resume-from {args.resume_from} requires existing {plan_file}")
        if start_idx >= _type_idx("review-code"):
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
        if start_idx >= _type_idx("address-code"):
            if not review_code_file.exists() or review_code_file.stat().st_size == 0:
                die(
                    f"--resume-from {args.resume_from} requires existing {review_code_file}"
                )

    pipe.run(*_signal_clients)

    # Accumulate cost from all client instances
    total_cost = 0.0
    for c in _spec_clients.values() if _spec_clients else [client] if client else []:
        total_cost += getattr(c, "total_cost_usd", 0.0) or 0.0
    if total_cost > 0:
        patch_state(gr_id, total_cost_usd=total_cost)

    logger.info("done. session artifacts in: %s", session_dir)
    if total_cost > 0:
        logger.info("total cost: $%.4f", total_cost)
    return 0


def _parse_review_args(argv: list[str]) -> argparse.Namespace:
    usage = (
        "usage: gremlins.cli review [--dir <path>] [--plan <path>] [-b <detail-model>]"
    )
    parser = argparse.ArgumentParser(add_help=False, usage=usage)
    parser.add_argument("--dir", dest="dir", default=".")
    parser.add_argument("--plan", dest="plan", default=None)
    parser.add_argument("-b", dest="detail", default=PACKAGE_DEFAULT.model)
    args = parser.parse_args(argv)
    return args


def review_main(argv: list[str], *, client: ClaudeClient | None = None) -> int:
    configure_logging()
    from gremlins.clients.claude import SubprocessClaudeClient

    if client is None:
        client = SubprocessClaudeClient()
    install_signal_handlers(client)
    args = _parse_review_args(argv)

    if shutil.which("claude") is None:
        die("claude CLI not found")

    session_dir = pathlib.Path(args.dir).resolve()
    if not session_dir.is_dir():
        die(f"--dir does not exist: {session_dir}")

    plan_text = ""
    if args.plan is not None:
        plan_path = pathlib.Path(args.plan)
        if not plan_path.exists():
            die(f"--plan does not exist: {plan_path}")
        if not plan_path.is_file():
            die(f"--plan is not a file: {plan_path}")
        if plan_path.stat().st_size == 0:
            die(f"--plan is empty: {plan_path}")
        try:
            plan_text = plan_path.read_text(encoding="utf-8")
        except UnicodeDecodeError:
            die(f"--plan is not valid UTF-8 text: {plan_path}")
        except OSError as exc:
            die(f"failed to read --plan {plan_path}: {exc}")

    is_git = in_git_repo()
    if is_git:
        head1_exists = rev_exists("HEAD~1")
        has_commit_diff = head1_exists and has_diff("HEAD~1", "HEAD")
        if not has_commit_diff and not has_dirty_worktree():
            if not head1_exists:
                die(
                    "nothing to review: no commit history beyond HEAD and working tree is clean"
                )
            die(
                "nothing to review: HEAD~1..HEAD has no changes and working tree is clean"
            )

    pipeline = load_pipeline(resolve_pipeline_path("local", pathlib.Path.cwd()))
    rc_entry = next((s for s in pipeline.stages if s.type == "review-code"), None)
    if rc_entry is None or not rc_entry.prompt_paths[1:]:
        die("local pipeline has no review-code stage with a prompt")
    ctx = StageContext(client=client, session_dir=session_dir, gr_id=None)
    logger.info("reviewing code (model: %s)", args.detail)
    stage = review_code.ReviewCode(
        rc_entry,
        args.detail,
        plan_text=plan_text,
        is_git=is_git,
    )
    stage.bind(ctx)
    review_file = stage.run(None)
    logger.info("code review (%s): %s", args.detail, review_file)
    return 0


def _parse_address_args(argv: list[str]) -> argparse.Namespace:
    usage = "usage: gremlins.cli address [--dir <path>] [-x <address-model>]"
    parser = argparse.ArgumentParser(add_help=False, usage=usage)
    parser.add_argument("--dir", dest="dir", default=".")
    parser.add_argument("-x", dest="address", default=PACKAGE_DEFAULT.model)
    args = parser.parse_args(argv)
    return args


def address_main(argv: list[str], *, client: ClaudeClient | None = None) -> int:
    configure_logging()
    from gremlins.clients.claude import SubprocessClaudeClient

    if client is None:
        client = SubprocessClaudeClient()
    install_signal_handlers(client)
    args = _parse_address_args(argv)

    if shutil.which("claude") is None:
        die("claude CLI not found")

    session_dir = pathlib.Path(args.dir).resolve()
    if not session_dir.is_dir():
        die(f"--dir does not exist: {session_dir}")

    is_git = in_git_repo()

    pipeline = load_pipeline(resolve_pipeline_path("local", pathlib.Path.cwd()))
    ac_entry = next((s for s in pipeline.stages if s.type == "address-code"), None)
    if ac_entry is None or not ac_entry.prompt_paths:
        die("local pipeline has no address-code stage with a prompt")

    ctx = StageContext(client=client, session_dir=session_dir, gr_id=None)
    logger.info("addressing code reviews (model: %s)", args.address)
    entry = dataclasses.replace(ac_entry, client=None)
    stage = address_code.AddressCode(
        entry,
        args.address,
        is_git=is_git,
    )
    stage.bind(ctx)
    stage.run(None)
    return 0
