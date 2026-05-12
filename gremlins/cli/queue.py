"""Thin CLI dispatch for gremlins queue."""

from __future__ import annotations

import argparse
import shlex
import sys

from gremlins.queue.core import add, clear, land, list_queue, requeue, run


def _add(argv: list[str]) -> int:
    if not argv:
        print("usage: gremlins queue add <command>", file=sys.stderr)
        return 1
    command = argv[0] if len(argv) == 1 else shlex.join(argv)
    name = add(command)
    print(f"queued: {name}")
    return 0


def _list(_argv: list[str]) -> int:
    return list_queue()


def _run(_argv: list[str]) -> int:
    return run()


def _requeue(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(prog="gremlins queue requeue")
    parser.add_argument("--done", action="store_true", help="Also requeue done items.")
    args = parser.parse_args(argv)
    return requeue(include_done=args.done)


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
    args = parser.parse_args(argv)
    return clear(
        failed_only=args.failed,
        done_only=args.done,
        pending_only=args.pending,
        purge=args.purge,
    )


def _land(_argv: list[str]) -> int:
    return land()


_DISPATCH = {
    "add": _add,
    "list": _list,
    "run": _run,
    "requeue": _requeue,
    "clear": _clear,
    "land": _land,
}


def queue_main(argv: list[str]) -> int:
    sub = argv[0] if argv else ""
    rest = argv[1:]
    handler = _DISPATCH.get(sub)
    if handler is None:
        print(
            "usage: gremlins queue <add|list|run|requeue|clear|land>", file=sys.stderr
        )
        return 1
    return handler(rest)
