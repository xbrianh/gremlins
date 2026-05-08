"""Orchestrator entry point for the gh pipeline."""

from __future__ import annotations

import argparse
import logging
import os
import pathlib
import re
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
    validate_stage_specs,
)
from gremlins.env_file import load_env_file
from gremlins.gh_utils import get_repo
from gremlins.logging_setup import configure_logging
from gremlins.orchestrators.base import read_state_field
from gremlins.orchestrators.gh_pipeline import GHPipeline
from gremlins.pipeline import (
    load_pipeline,
    resolve_pipeline_path,
)
from gremlins.state import (
    patch_state,
    resolve_session_dir,
    resolve_state_file,
)

logger = logging.getLogger(__name__)

REF_RE = re.compile(r"^[A-Za-z0-9._/#-]+$")


def die(msg: str) -> NoReturn:
    sys.stderr.write(f"error: {msg}\n")
    sys.stderr.flush()
    sys.exit(1)


def _parse_gh_args(argv: list[str]) -> argparse.Namespace:
    usage = (
        "usage: gremlins.cli gh [-r <ref>] [--resume-from <stage>] "
        "[--plan <path|issue-ref>] [--spec <path>] "
        '[--pipeline <name-or-path>] [--client <provider:model>] "<instructions>"'
    )
    parser = argparse.ArgumentParser(add_help=False, usage=usage)
    parser.add_argument("-r", dest="ref", default="")
    parser.add_argument("--resume-from", dest="resume_from", default=None)
    parser.add_argument("--plan", dest="plan_source", default=None)
    parser.add_argument("--spec", dest="spec_path", default=None)
    parser.add_argument("--pipeline", dest="pipeline", default=None)
    parser.add_argument("--client", dest="client", default=None)
    parser.add_argument("instructions", nargs="*")
    args = parser.parse_args(argv)

    if args.plan_source:
        if args.instructions:
            die("--plan and positional instructions are mutually exclusive")
    else:
        if args.resume_from is None and not args.instructions:
            die(usage)

    if args.ref and not REF_RE.match(args.ref):
        die(f"invalid -r ref: {args.ref} (allowed: A-Z a-z 0-9 . _ / # -)")

    return args


def gh_main(
    argv: list[str], *, client: ClaudeClient | None = None, gr_id: str | None = None
) -> int:
    configure_logging()
    args = _parse_gh_args(argv)
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
    if shutil.which("gh") is None:
        die("gh CLI not found")

    cli_spec: ClientSpec | None = None
    if args.client:
        try:
            cli_spec = ClientSpec.parse(args.client)
        except ValueError as exc:
            die(str(exc))

    try:
        pipeline = load_pipeline(
            resolve_pipeline_path(args.pipeline or "gh", pathlib.Path.cwd())
        )
    except (FileNotFoundError, ValueError, yaml.YAMLError) as exc:
        die(str(exc))

    # Load or resolve stage specs; state.json is authoritative on resume
    stage_specs: dict[str, ClientSpec] = {}
    state_file = resolve_state_file(gr_id)
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
    effective_client = client if client is not None else _client_for_spec(default_spec)

    session_dir = resolve_session_dir(gr_id)

    if client is not None:
        _signal_clients = [client]
    elif _spec_clients:
        _signal_clients = list(_spec_clients.values())
    else:
        _signal_clients = [effective_client]

    try:
        validate_stage_specs(stage_specs, pipeline)
    except ValueError as exc:
        die(str(exc))

    repo = get_repo()
    spec_file = session_dir / "spec.md"

    logger.info("session: %s", session_dir)

    if args.spec_path and not spec_file.exists():
        spec_src = pathlib.Path(args.spec_path)
        if not spec_src.is_file():
            die(f"--spec: file not found: {args.spec_path}")
        if spec_src.stat().st_size == 0:
            die(f"--spec: file is empty: {args.spec_path}")
        shutil.copyfile(spec_src, spec_file)

    try:
        pipe = GHPipeline(
            pipeline.stages,
            args=args,
            session_dir=session_dir,
            gr_id=gr_id,
            pipeline_data=pipeline,
            repo=repo,
            state_file=state_file,
            spec_clients=_spec_clients,
            stage_specs=stage_specs,
            test_client=client,
        )
        pipe.validate_resume_target()
    except ValueError as exc:
        die(str(exc))

    try:
        pipe.run(*_signal_clients)
    except ValueError as exc:
        die(str(exc))

    total_cost = 0.0
    for c in _spec_clients.values() if _spec_clients else [client] if client else []:
        total_cost += getattr(c, "total_cost_usd", 0.0) or 0.0
    if total_cost > 0:
        patch_state(gr_id, total_cost_usd=total_cost)

    logger.info("done. PR: %s", read_state_field(state_file, "pr_url") or "(unknown)")
    if total_cost > 0:
        logger.info("total cost: $%.4f", total_cost)
    return 0
