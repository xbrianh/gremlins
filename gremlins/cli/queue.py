"""Thin CLI dispatch for gremlins queue."""

from __future__ import annotations

import argparse
import shlex
import sys
from collections.abc import Callable

from gremlins.queue.core import (
    SUBDIRS,
    add,
    clear,
    list_queue,
    list_queue_json,
    requeue,
    run,
    runner_active,
    set_state,
)
from gremlins.utils.watch import watch_render


def _add(argv: list[str]) -> int:
    if not argv or argv[0] in ("-h", "--help"):
        print("usage: gremlins queue add <command>", file=sys.stderr)
        return 0 if argv else 1
    command = argv[0] if len(argv) == 1 else shlex.join(argv)
    name = add(command)
    print(f"queued: {name}")
    if runner_active():
        print("runner: active")
    else:
        print("warning: no runner active — start with: gremlins queue run")
    return 0


def _list(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(prog="gremlins queue list")
    parser.add_argument(
        "--watch",
        nargs="?",
        const=2,
        type=int,
        metavar="SEC",
        help="Refresh every SEC seconds (default 2). Mutually exclusive with --json.",
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="Emit JSON instead of human-formatted output. Mutually exclusive with --watch.",
    )
    args = parser.parse_args(argv)
    if args.json and args.watch is not None:
        print("error: --json cannot be combined with --watch", file=sys.stderr)
        return 1
    if args.json:
        return list_queue_json()
    if args.watch is not None:
        return watch_render(args.watch, list_queue)
    return list_queue()


def _run(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(prog="gremlins queue run")
    parser.add_argument(
        "--once",
        action="store_true",
        help="Exit when pending/ is empty instead of watching for new items.",
    )
    parser.add_argument(
        "--poll-interval",
        type=float,
        default=1.0,
        metavar="SEC",
        help="Seconds between pending-dir polls when watching (default 1).",
    )
    try:
        args = parser.parse_args(argv)
    except SystemExit as exc:
        return int(exc.code or 0)
    if args.poll_interval <= 0:
        print("queue run: --poll-interval must be > 0", file=sys.stderr)
        return 1
    return run(once=args.once, poll_interval=args.poll_interval)


def _requeue(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(prog="gremlins queue requeue")
    parser.add_argument("--done", action="store_true", help="Also requeue done items.")
    args = parser.parse_args(argv)
    rc = requeue(include_done=args.done)
    if rc == 0:
        if runner_active():
            print("runner: active")
        else:
            print("warning: no runner active — start with: gremlins queue run")
    return rc


def _clear(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(prog="gremlins queue clear")
    scope = parser.add_mutually_exclusive_group()
    scope.add_argument("--failed", action="store_true", help="Clear only failed items.")
    scope.add_argument("--done", action="store_true", help="Clear only done items.")
    scope.add_argument(
        "--pending", action="store_true", help="Clear only pending items."
    )
    scope.add_argument(
        "--purge",
        action="store_true",
        help="Empty all 4 dirs and stop running gremlins.",
    )
    scope.add_argument("--item", metavar="STEM", help="Remove a single item by stem.")
    args = parser.parse_args(argv)
    return clear(
        failed_only=args.failed,
        done_only=args.done,
        pending_only=args.pending,
        purge=args.purge,
        item=args.item,
    )


def _set_state(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(prog="gremlins queue set-state")
    parser.add_argument("state", choices=SUBDIRS, help="Target state.")
    parser.add_argument(
        "--item", metavar="STEM", required=True, help="Queue item stem."
    )
    args = parser.parse_args(argv)
    return set_state(args.item, args.state)


_DISPATCH: dict[str, tuple[str, Callable[[list[str]], int]]] = {
    "add": ("Add a command to the queue.", _add),
    "list": ("List queued items.", _list),
    "run": ("Run the next item in the queue.", _run),
    "requeue": ("Move failed items back to pending.", _requeue),
    "clear": ("Remove items from the queue.", _clear),
    "set-state": ("Manually transition a queue item to a different state.", _set_state),
}


def _build_queue_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(
        prog="gremlins queue",
        description="Manage the gremlin launch queue.",
    )
    subs = p.add_subparsers(title="subcommands", metavar="subcommand")
    for name, (help_text, _) in _DISPATCH.items():
        subs.add_parser(name, help=help_text)
    return p


def queue_main(argv: list[str]) -> int:
    sub = argv[0] if argv else ""
    if sub in ("-h", "--help") or not sub:
        _build_queue_parser().print_help()
        return 0 if sub in ("-h", "--help") else 1
    entry = _DISPATCH.get(sub)
    if entry is None:
        _build_queue_parser().print_help(sys.stderr)
        return 1
    _, handler = entry
    return handler(argv[1:])
