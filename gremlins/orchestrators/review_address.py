"""Stage-level CLI commands: review and address."""

from __future__ import annotations

import argparse
import dataclasses
import logging
import pathlib
import shutil
import sys
from typing import NoReturn

from gremlins.clients.protocol import ClaudeClient
from gremlins.clients.resolve import PACKAGE_DEFAULT
from gremlins.git import has_diff, has_dirty_worktree, in_git_repo, rev_exists
from gremlins.logging_setup import configure_logging
from gremlins.pipeline import load_pipeline, resolve_pipeline_path
from gremlins.runner import install_signal_handlers
from gremlins.stages import address_code, review_code
from gremlins.stages.base import StageContext

logger = logging.getLogger(__name__)


def _die(msg: str) -> NoReturn:
    sys.stderr.write(f"error: {msg}\n")
    sys.stderr.flush()
    sys.exit(1)


def _parse_review_args(argv: list[str]) -> argparse.Namespace:
    usage = (
        "usage: gremlins.cli review [--dir <path>] [--plan <path>] [-b <detail-model>]"
    )
    parser = argparse.ArgumentParser(add_help=False, usage=usage)
    parser.add_argument("--dir", dest="dir", default=".")
    parser.add_argument("--plan", dest="plan", default=None)
    parser.add_argument("-b", dest="detail", default=PACKAGE_DEFAULT.model)
    return parser.parse_args(argv)


def review_main(argv: list[str], *, client: ClaudeClient | None = None) -> int:
    configure_logging()
    from gremlins.clients.claude import SubprocessClaudeClient

    if client is None:
        client = SubprocessClaudeClient()
    install_signal_handlers(client)
    args = _parse_review_args(argv)

    if shutil.which("claude") is None:
        _die("claude CLI not found")

    session_dir = pathlib.Path(args.dir).resolve()
    if not session_dir.is_dir():
        _die(f"--dir does not exist: {session_dir}")

    plan_text = ""
    if args.plan is not None:
        plan_path = pathlib.Path(args.plan)
        if not plan_path.exists():
            _die(f"--plan does not exist: {plan_path}")
        if not plan_path.is_file():
            _die(f"--plan is not a file: {plan_path}")
        if plan_path.stat().st_size == 0:
            _die(f"--plan is empty: {plan_path}")
        try:
            plan_text = plan_path.read_text(encoding="utf-8")
        except UnicodeDecodeError:
            _die(f"--plan is not valid UTF-8 text: {plan_path}")
        except OSError as exc:
            _die(f"failed to read --plan {plan_path}: {exc}")

    is_git = in_git_repo()
    if is_git:
        head1_exists = rev_exists("HEAD~1")
        has_commit_diff = head1_exists and has_diff("HEAD~1", "HEAD")
        if not has_commit_diff and not has_dirty_worktree():
            if not head1_exists:
                _die(
                    "nothing to review: no commit history beyond HEAD and working tree is clean"
                )
            _die(
                "nothing to review: HEAD~1..HEAD has no changes and working tree is clean"
            )

    pipeline = load_pipeline(resolve_pipeline_path("local", pathlib.Path.cwd()))
    rc_entry = next((s for s in pipeline.stages if s.type == "review-code"), None)
    if rc_entry is None or not rc_entry.prompt_paths[1:]:
        _die("local pipeline has no review-code stage with a prompt")
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
    return parser.parse_args(argv)


def address_main(argv: list[str], *, client: ClaudeClient | None = None) -> int:
    configure_logging()
    from gremlins.clients.claude import SubprocessClaudeClient

    if client is None:
        client = SubprocessClaudeClient()
    install_signal_handlers(client)
    args = _parse_address_args(argv)

    if shutil.which("claude") is None:
        _die("claude CLI not found")

    session_dir = pathlib.Path(args.dir).resolve()
    if not session_dir.is_dir():
        _die(f"--dir does not exist: {session_dir}")

    is_git = in_git_repo()

    pipeline = load_pipeline(resolve_pipeline_path("local", pathlib.Path.cwd()))
    ac_entry = next((s for s in pipeline.stages if s.type == "address-code"), None)
    if ac_entry is None or not ac_entry.prompt_paths:
        _die("local pipeline has no address-code stage with a prompt")

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
