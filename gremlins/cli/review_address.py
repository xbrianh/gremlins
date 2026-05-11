from __future__ import annotations

import argparse
import logging
import pathlib
import shutil

from gremlins.clients.client import PACKAGE_DEFAULT, Client
from gremlins.errors import die
from gremlins.logging_setup import configure_logging
from gremlins.orchestrators.review_address import run_address, run_review
from gremlins.runner import install_signal_handlers

logger = logging.getLogger(__name__)


def _parse_review_args(argv: list[str]) -> argparse.Namespace:
    usage = (
        "usage: gremlins review [--dir <path>] [--plan <path>] [-b <detail-model>]"
    )
    parser = argparse.ArgumentParser(add_help=False, usage=usage)
    parser.add_argument("--dir", dest="dir", default=".")
    parser.add_argument("--plan", dest="plan", default=None)
    parser.add_argument("-b", dest="detail", default=PACKAGE_DEFAULT.model)
    return parser.parse_args(argv)


def review_main(argv: list[str], *, client: Client | None = None) -> int:
    configure_logging()
    if client is None:
        client = PACKAGE_DEFAULT
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

    return run_review(session_dir, plan_text, args.detail, client)


def _parse_address_args(argv: list[str]) -> argparse.Namespace:
    usage = "usage: gremlins address [--dir <path>] [-x <address-model>]"
    parser = argparse.ArgumentParser(add_help=False, usage=usage)
    parser.add_argument("--dir", dest="dir", default=".")
    parser.add_argument("-x", dest="address", default=PACKAGE_DEFAULT.model)
    return parser.parse_args(argv)


def address_main(argv: list[str], *, client: Client | None = None) -> int:
    configure_logging()
    if client is None:
        client = PACKAGE_DEFAULT
    install_signal_handlers(client)
    args = _parse_address_args(argv)

    if shutil.which("claude") is None:
        die("claude CLI not found")

    session_dir = pathlib.Path(args.dir).resolve()
    if not session_dir.is_dir():
        die(f"--dir does not exist: {session_dir}")

    return run_address(session_dir, args.address, client)
