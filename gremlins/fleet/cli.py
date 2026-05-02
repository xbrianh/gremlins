"""Argument parsing and main entry point."""

import argparse
import os
import signal
import sys
import time
import types

from gremlins.fleet import constants as _constants
from gremlins.fleet.close import do_close
from gremlins.fleet.land import do_land, do_rm
from gremlins.fleet.log import do_log
from gremlins.fleet.rescue import do_rescue
from gremlins.fleet.state import git_toplevel
from gremlins.fleet.stop import do_stop
from gremlins.fleet.views import do_drill_in, do_list, do_recent


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        prog="gremlins.sh",
        description="On-demand status of background gremlins.",
        epilog=(
            "Subcommands (positional, before flags):\n"
            "  stop <id>     Send SIGTERM to a running gremlin and wait for it to exit.\n"
            "  rescue <id>   Diagnose and resume a dead or stalled gremlin inline.\n"
            "                Pass --headless to run end-to-end with no TTY: refuses\n"
            "                excluded bail classes, caps at 3 attempts, writes a\n"
            "                bail_reason to state.json on bail.\n"
            "  rm <id>       Delete a dead/finished gremlin's state directory, worktree, and branch.\n"
            "  close <id>    Mark a dead/finished gremlin as closed (hides it from the default view).\n"
            "  log <id>      Tail the gremlin's log file (`tail -F`). Ctrl-C exits.\n"
            "  land <id>     Land a finished gremlin onto the current branch, then clean up.\n"
            "                Default mode: localgremlin → --squash, bossgremlin → --ff.\n"
            "                gh → merges the PR (mode flags not applicable).\n"
            "                --squash: collapse all commits above the merge base into one.\n"
            "                --ff:     fast-forward the current branch (hard fail if diverged).\n"
            "                --squash and --ff are mutually exclusive.\n"
            "                Preserves the state directory — use 'rm' for full cleanup.\n"
            "                Pass --force to skip merge and clean up a closed gh PR.\n"
        ),
        formatter_class=argparse.RawDescriptionHelpFormatter,
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
        "--kind",
        choices=["local", "gh", "boss"],
        metavar="local|gh|boss",
        help="Filter to a specific gremlin kind.",
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
    return parser.parse_args(argv)


def render_view(args: argparse.Namespace, here_root: str | None) -> None:
    """Render whichever view the flags request. Used by both normal and --watch path."""
    if args.recent is not None and (args.running or args.dead or args.stalled):
        print(
            "error: --recent cannot be combined with --running/--dead/--stalled",
            file=sys.stderr,
        )
        return

    if args.recent is not None:
        do_recent(args, here_root=here_root)
    else:
        do_list(args, here_root=here_root)


def _dispatch_subcommand(argv: list[str]):
    """
    Pre-process argv for stop/rescue subcommands before argparse runs.
    Returns (handled: bool, ok: bool). handled=False means not a subcommand.
    """
    raw = list(argv)
    non_flags = [a for a in raw if not a.startswith("-")]
    if not non_flags or non_flags[0] not in (
        "stop",
        "rescue",
        "rm",
        "close",
        "land",
        "log",
    ):
        return False, False

    subcommand = non_flags[0]
    # Find the index of the subcommand in raw argv and take the next non-flag.
    sc_idx = next(i for i, a in enumerate(raw) if a == subcommand)
    trailing = [a for a in raw[sc_idx + 1 :] if not a.startswith("-")]
    if not trailing:
        print(f"usage: gremlins {subcommand} <id-prefix>")
        sys.exit(1)

    target = trailing[0]
    if not os.path.isdir(_constants.STATE_ROOT):
        print("No gremlins have been launched on this machine.")
        sys.exit(0)

    if subcommand == "stop":
        ok = do_stop(target)
    elif subcommand == "rm":
        ok = do_rm(target)
    elif subcommand == "close":
        ok = do_close(target)
    elif subcommand == "log":
        ok = do_log(target)
    elif subcommand == "land":
        force = "--force" in raw
        squash_flag = "--squash" in raw
        ff_flag = "--ff" in raw
        if squash_flag and ff_flag:
            print("error: --squash and --ff are mutually exclusive")
            sys.exit(1)
        mode = "squash" if squash_flag else ("ff" if ff_flag else None)
        into_dir = ""
        if "--into" in raw:
            into_idx = raw.index("--into")
            if into_idx + 1 < len(raw):
                into_dir = raw[into_idx + 1]
            else:
                print("error: --into requires a directory argument")
                sys.exit(1)
        ok = do_land(target, force=force, mode=mode, into_dir=into_dir)
    else:
        headless = "--headless" in raw
        ok = do_rescue(target, headless=headless)
    return True, ok


def _main_impl(argv: list[str] | None = None) -> int:
    if argv is None:
        argv = sys.argv[1:]
    handled, ok = _dispatch_subcommand(argv)
    if handled:
        sys.exit(0 if ok else 1)

    args = parse_args(argv)

    # --watch and positional drill-in are mutually exclusive.
    if args.watch is not None and args.id_prefix is not None:
        print("error: --watch cannot be combined with a positional id argument")
        sys.exit(0)

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
        do_drill_in(args.id_prefix)
        sys.exit(0)

    # --watch loop.
    if args.watch is not None:
        interval = max(1, args.watch)
        stop = [False]

        def _handle_sigint(signum: int, frame: types.FrameType | None) -> None:
            stop[0] = True

        signal.signal(signal.SIGINT, _handle_sigint)

        while not stop[0]:
            sys.stdout.write("\033[2J\033[H")
            sys.stdout.flush()
            render_view(args, here_root)
            for _ in range(interval * 10):
                if stop[0]:
                    break
                time.sleep(0.1)
        sys.exit(0)

    # Default: single render.
    render_view(args, here_root)
    sys.exit(0)


def main(argv: list[str] | None = None) -> int:
    """Entry point. Wraps ``_main_impl`` in a top-level try/except so the
    "exit 0 on the listing path even on unexpected errors" promise from the
    module docstring holds regardless of how this is invoked — via
    ``python -m gremlins.cli fleet`` dispatch or import + call from a test.

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
