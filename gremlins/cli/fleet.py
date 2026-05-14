"""Argument parsing and main entry point."""

import argparse
import os
import sys

import gremlins.fleet.constants as _constants
from gremlins.fleet.ack import do_ack, do_skip
from gremlins.fleet.close import do_close
from gremlins.fleet.land import do_land, do_rm
from gremlins.fleet.log import do_log
from gremlins.fleet.rescue import do_rescue
from gremlins.fleet.state import git_toplevel
from gremlins.fleet.stop import do_stop
from gremlins.fleet.views import (
    do_drill_in,
    do_drill_in_json,
    do_list,
    do_list_json,
    do_recent,
)
from gremlins.utils.watch import watch_render


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        prog="gremlins",
        description="On-demand status of background gremlins.",
        add_help=True,
    )
    parser.add_argument(
        "--here",
        action="store_true",
        help="Only gremlins whose project_root matches this repo.",
    )
    parser.add_argument(
        "--running",
        action="store_true",
        help="Show only running gremlins.",
    )
    parser.add_argument(
        "--dead",
        action="store_true",
        help="Show only dead gremlins.",
    )
    parser.add_argument(
        "--stalled",
        action="store_true",
        help="Show only stalled gremlins.",
    )
    parser.add_argument(
        "--pipeline",
        metavar="NAME",
        help="Filter to gremlins using this pipeline (substring match against pipeline name).",
    )
    parser.add_argument(
        "--since",
        metavar="DURATION",
        help="Show only gremlins started within DURATION (e.g. 30s, 5m, 2h, 1d).",
    )
    parser.add_argument(
        "--recent",
        nargs="?",
        const=24,
        type=int,
        metavar="N",
        help="Show recently-finished gremlins started within N hours (default 24). "
        "Mutually exclusive with --running/--dead/--stalled.",
    )
    parser.add_argument(
        "--watch",
        nargs="?",
        const=2,
        type=int,
        metavar="SEC",
        help="Refresh the view every SEC seconds (default 2). "
        "Mutually exclusive with positional id argument.",
    )
    parser.add_argument(
        "id_prefix",
        nargs="?",
        metavar="id-prefix",
        help="Substring to drill into a single gremlin. Mutually exclusive with --watch.",
    )
    parser.add_argument(
        "--all",
        action="store_true",
        help=argparse.SUPPRESS,
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="Emit JSON instead of human-formatted output. Mutually exclusive with --watch.",
    )
    return parser.parse_args(argv)


def render_view(args: argparse.Namespace, here_root: str | None) -> None:
    """Render whichever view the flags request. Used by both normal and --watch path."""
    if args.recent is not None and (args.running or args.dead or args.stalled):
        print(
            "error: --recent cannot be combined with --running/--dead/--stalled",
            file=sys.stderr,
        )
        return

    if args.json:
        do_list_json(args, here_root=here_root)
    elif args.recent is not None:
        do_recent(args, here_root=here_root)
    else:
        do_list(args, here_root=here_root)


def _no_state_root() -> bool:
    if not os.path.isdir(_constants.STATE_ROOT):
        print("No gremlins have been launched on this machine.")
        return True
    return False


def stop_main(argv: list[str]) -> int:
    p = argparse.ArgumentParser(
        prog="gremlins stop",
        description="Send SIGTERM to a running gremlin and wait for it to exit.",
    )
    p.add_argument("id_prefix", metavar="id-prefix")
    args = p.parse_args(argv)
    if _no_state_root():
        return 0
    return 0 if do_stop(args.id_prefix) else 1


def rescue_main(argv: list[str]) -> int:
    p = argparse.ArgumentParser(
        prog="gremlins rescue",
        description="Diagnose and resume a dead or stalled gremlin.",
    )
    p.add_argument("id_prefix", metavar="id-prefix")
    p.add_argument(
        "--headless", action="store_true", help="Run end-to-end with no TTY."
    )
    p.add_argument("--from-boss", action="store_true", help="Called from a boss chain.")
    args = p.parse_args(argv)
    if _no_state_root():
        return 0
    return (
        0
        if do_rescue(args.id_prefix, headless=args.headless, from_boss=args.from_boss)
        else 1
    )


def rm_main(argv: list[str]) -> int:
    p = argparse.ArgumentParser(
        prog="gremlins rm",
        description="Delete a gremlin's state directory, worktree, and branch.",
    )
    p.add_argument("id_prefix", metavar="id-prefix")
    args = p.parse_args(argv)
    if _no_state_root():
        return 0
    return 0 if do_rm(args.id_prefix) else 1


def close_main(argv: list[str]) -> int:
    p = argparse.ArgumentParser(
        prog="gremlins close",
        description="Mark a gremlin as closed (hidden from default view).",
    )
    p.add_argument("id_prefix", metavar="id-prefix")
    args = p.parse_args(argv)
    if _no_state_root():
        return 0
    return 0 if do_close(args.id_prefix) else 1


def log_main(argv: list[str]) -> int:
    p = argparse.ArgumentParser(
        prog="gremlins log",
        description="Tail the gremlin's log file (tail -F). Ctrl-C exits.",
    )
    p.add_argument("id_prefix", metavar="id-prefix")
    args = p.parse_args(argv)
    if _no_state_root():
        return 0
    return 0 if do_log(args.id_prefix) else 1


def land_main(argv: list[str]) -> int:
    p = argparse.ArgumentParser(
        prog="gremlins land",
        description="Land a finished gremlin onto the current branch, then clean up.",
    )
    p.add_argument("id_prefix", metavar="id-prefix")
    mode = p.add_mutually_exclusive_group()
    mode.add_argument(
        "--squash", action="store_true", help="Collapse commits into one."
    )
    mode.add_argument(
        "--ff", action="store_true", help="Fast-forward (fails if diverged)."
    )
    p.add_argument(
        "--force", action="store_true", help="Skip merge and clean up a closed gh PR."
    )
    p.add_argument(
        "--into", metavar="DIR", default="", help="Target directory for the merge."
    )
    args = p.parse_args(argv)
    if _no_state_root():
        return 0
    land_mode = "squash" if args.squash else ("ff" if args.ff else None)
    return (
        0
        if do_land(args.id_prefix, force=args.force, mode=land_mode, into_dir=args.into)
        else 1
    )


def ack_main(argv: list[str]) -> int:
    p = argparse.ArgumentParser(
        prog="gremlins ack",
        description="Acknowledge a gremlin waiting for human input.",
    )
    p.add_argument("id_prefix", metavar="id-prefix")
    args = p.parse_args(argv)
    if _no_state_root():
        return 0
    return 0 if do_ack(args.id_prefix) else 1


def skip_main(argv: list[str]) -> int:
    p = argparse.ArgumentParser(
        prog="gremlins skip",
        description="Skip a gremlin waiting for human input.",
    )
    p.add_argument("id_prefix", metavar="id-prefix")
    args = p.parse_args(argv)
    if _no_state_root():
        return 0
    return 0 if do_skip(args.id_prefix) else 1


def _main_impl(argv: list[str] | None = None) -> int:
    if argv is None:
        argv = sys.argv[1:]

    args = parse_args(argv)

    # --watch and positional drill-in are mutually exclusive.
    if args.watch is not None and args.id_prefix is not None:
        print("error: --watch cannot be combined with a positional id argument")
        sys.exit(0)

    # --json and --watch are mutually exclusive.
    if args.json and args.watch is not None:
        print("error: --json cannot be combined with --watch", file=sys.stderr)
        sys.exit(1)

    # Early exit if state root doesn't exist.
    if not os.path.isdir(_constants.STATE_ROOT):
        print("No gremlins have been launched on this machine.")
        sys.exit(0)

    # Resolve --here once.
    here_root = None
    if args.here:
        here_root = git_toplevel()

    # Drill-in positional argument (no --watch).
    if args.id_prefix is not None:
        if args.json:
            do_drill_in_json(args.id_prefix)
        else:
            do_drill_in(args.id_prefix)
        sys.exit(0)

    # --watch loop.
    if args.watch is not None:
        sys.exit(watch_render(args.watch, lambda: render_view(args, here_root)))

    # Default: single render.
    render_view(args, here_root)
    sys.exit(0)


def fleet_main(argv: list[str] | None = None) -> int:
    """Entry point. Wraps ``_main_impl`` in a top-level try/except so the
    "exit 0 on the listing path even on unexpected errors" promise from the
    module docstring holds regardless of how this is invoked — bare ``gremlins``
    invocation or import + call from a test.

    ``SystemExit`` is re-raised verbatim so deliberate ``sys.exit(N)`` calls
    inside ``_main_impl`` (including ``sys.exit(1)`` for handled failures)
    keep their intended exit codes; only unexpected exceptions are swallowed.
    """
    try:
        return _main_impl(argv)
    except SystemExit:
        raise
    except Exception as exc:
        print(f"gremlins: unexpected error: {exc}", file=sys.stderr)
        sys.exit(0)
