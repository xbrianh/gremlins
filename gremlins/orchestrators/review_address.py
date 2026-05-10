"""Stage-level CLI commands: review and address."""

from __future__ import annotations

import argparse
import logging
import pathlib
import shutil

import gremlins.stages.address_code as address_code
import gremlins.stages.review_code as review_code
from gremlins.clients.client import Client, PACKAGE_DEFAULT
from gremlins.errors import die
from gremlins.git import has_diff, has_dirty_worktree, in_git_repo, rev_exists
from gremlins.logging_setup import configure_logging
from gremlins.pipeline.discovery import resolve_pipeline_path
from gremlins.pipeline.loader import load_pipeline
from gremlins.runner import install_signal_handlers
from gremlins.stages.base import StageContext

logger = logging.getLogger(__name__)


def _parse_review_args(argv: list[str]) -> argparse.Namespace:
    usage = (
        "usage: gremlins.cli review [--dir <path>] [--plan <path>] [-b <detail-model>]"
    )
    parser = argparse.ArgumentParser(add_help=False, usage=usage)
    parser.add_argument("--dir", dest="dir", default=".")
    parser.add_argument("--plan", dest="plan", default=None)
    parser.add_argument("-b", dest="detail", default=PACKAGE_DEFAULT.model)
    return parser.parse_args(argv)


def review_main(argv: list[str], *, client: Client | None = None) -> int:
    configure_logging()
    if client is None:
        client = Client("claude", "sonnet")
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
    if rc_entry is None or not rc_entry.prompts[1:]:
        die("local pipeline has no review-code stage with a prompt")
    ctx = StageContext(client=client, session_dir=session_dir, gr_id=None)
    logger.info("reviewing code (model: %s)", args.detail)
    stage = review_code.ReviewCode(
        rc_entry.name,
        args.detail,
        rc_entry.prompts,
        rc_entry.options,
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


def address_main(argv: list[str], *, client: Client | None = None) -> int:
    configure_logging()
    if client is None:
        client = Client("claude", "sonnet")
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
    if ac_entry is None or not ac_entry.prompts:
        die("local pipeline has no address-code stage with a prompt")

    ctx = StageContext(client=client, session_dir=session_dir, gr_id=None)
    logger.info("addressing code reviews (model: %s)", args.address)
    stage = address_code.AddressCode(
        ac_entry.name,
        args.address,
        ac_entry.prompts,
        ac_entry.options,
        is_git=is_git,
    )
    stage.bind(ctx)
    stage.run(None)
    return 0
