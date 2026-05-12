"""Top-level dispatch for ``python -m gremlins.cli``."""

from __future__ import annotations

import argparse
import sys

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

_SUBCOMMAND_HELP = {
    "launch": "Launch a background gremlin by pipeline name.",
    "resume": "Re-spawn an existing gremlin from its recorded stage.",
    "stop": "Send SIGTERM to a running gremlin and wait for it to exit.",
    "rescue": "Diagnose and resume a dead or stalled gremlin.",
    "land": "Land a finished gremlin onto the current branch, then clean up.",
    "rm": "Delete a gremlin's state directory, worktree, and branch.",
    "close": "Mark a gremlin as closed (hidden from default view).",
    "log": "Tail the gremlin's log file (tail -F). Ctrl-C exits.",
    "ack": "Acknowledge a gremlin waiting for human input.",
    "skip": "Skip a gremlin waiting for human input.",
    "prompt-for-assistant": "Print the assistant setup prompt to stdout.",
    "queue": "Manage the gremlin launch queue.",
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
    for name, help_text in _SUBCOMMAND_HELP.items():
        subs.add_parser(name, help=help_text)
    return parser


def main(argv: list[str] | None = None) -> int:
    if argv is None:
        argv = sys.argv[1:]

    sub = argv[0] if argv else ""

    if sub in ("-h", "--help"):
        _build_parser().print_help()
        return 0

    rest = argv[1:]
    if sub == "launch":
        return launch_main(rest)
    if sub == "resume":
        return resume_main(rest)
    if sub == "stop":
        return stop_main(rest)
    if sub == "rescue":
        return rescue_main(rest)
    if sub == "land":
        return land_main(rest)
    if sub == "rm":
        return rm_main(rest)
    if sub == "close":
        return close_main(rest)
    if sub == "log":
        return log_main(rest)
    if sub == "ack":
        return ack_main(rest)
    if sub == "skip":
        return skip_main(rest)
    if sub == "prompt-for-assistant":
        return prompt_for_assistant_main(rest)
    if sub == "queue":
        return queue_main(rest)

    return fleet_main(argv)
