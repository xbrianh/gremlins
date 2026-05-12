"""Top-level dispatch for ``python -m gremlins.cli``."""

from __future__ import annotations

import argparse
import sys
from typing import Callable

from gremlins.cli.fleet import (
    ack_main,
    close_main,
    fleet_main,
    land_main,
    log_main,
    rescue_main,
    rm_main,
    skip_main,
    stop_main,
)
from gremlins.cli.launch import launch_main
from gremlins.cli.prompt_for_assistant import prompt_for_assistant_main
from gremlins.cli.queue import queue_main
from gremlins.cli.resume import resume_main

_DISPATCH: dict[str, tuple[str, Callable[[list[str]], int]]] = {
    "launch": ("Launch a background gremlin by pipeline name.", launch_main),
    "resume": ("Re-spawn an existing gremlin from its recorded stage.", resume_main),
    "stop": ("Send SIGTERM to a running gremlin and wait for it to exit.", stop_main),
    "rescue": ("Diagnose and resume a dead or stalled gremlin.", rescue_main),
    "land": ("Land a finished gremlin onto the current branch, then clean up.", land_main),
    "rm": ("Delete a gremlin's state directory, worktree, and branch.", rm_main),
    "close": ("Mark a gremlin as closed (hidden from default view).", close_main),
    "log": ("Tail the gremlin's log file (tail -F). Ctrl-C exits.", log_main),
    "ack": ("Acknowledge a gremlin waiting for human input.", ack_main),
    "skip": ("Skip a gremlin waiting for human input.", skip_main),
    "prompt-for-assistant": ("Print the assistant setup prompt to stdout.", prompt_for_assistant_main),
    "queue": ("Manage the gremlin launch queue.", queue_main),
}


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="gremlins",
        description="Launch and manage background gremlins.",
        epilog=(
            "No subcommand: show fleet status view.\n"
            "Run 'gremlins <subcommand> --help' for subcommand-specific options."
        ),
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    subs = parser.add_subparsers(title="subcommands", metavar="subcommand")
    for name, (help_text, _) in _DISPATCH.items():
        subs.add_parser(name, help=help_text)
    return parser


def main(argv: list[str] | None = None) -> int:
    if argv is None:
        argv = sys.argv[1:]

    sub = argv[0] if argv else ""

    if sub in ("-h", "--help"):
        _build_parser().print_help()
        return 0

    entry = _DISPATCH.get(sub)
    if entry is not None:
        _, handler = entry
        return handler(argv[1:])

    return fleet_main(argv)
