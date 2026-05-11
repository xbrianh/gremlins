"""CLI entry point for emitting a bail marker from inside a running pipeline stage."""

from __future__ import annotations

import argparse
import os
import sys

from gremlins.executor.state import (
    BAIL_CLASS_OTHER,
    BAIL_CLASS_REVIEWER_REQUESTED_CHANGES,
    BAIL_CLASS_SECRETS,
    BAIL_CLASS_SECURITY,
    emit_bail,
    validate_gr_id,
)

_VALID = {
    BAIL_CLASS_REVIEWER_REQUESTED_CHANGES,
    BAIL_CLASS_SECURITY,
    BAIL_CLASS_SECRETS,
    BAIL_CLASS_OTHER,
}


def _resolve_child_key(arg_child_key: str | None) -> str | None:
    if arg_child_key:
        return arg_child_key
    env_child_key = os.environ.get("GREMLIN_CHILD_KEY")
    return env_child_key or None


def bail_main(argv: list[str] | None = None) -> int:
    if argv is None:
        argv = sys.argv[1:]
    p = argparse.ArgumentParser(
        prog="python -m gremlins.bail",
        description="Mark the running gremlin as bailed.",
    )
    p.add_argument("--child-key")
    p.add_argument("bail_class", choices=sorted(_VALID))
    p.add_argument("bail_detail", nargs="?", default="")
    args = p.parse_args(argv)

    gr_id = os.environ.get("GR_ID")
    if gr_id is not None:
        try:
            validate_gr_id(gr_id)
        except ValueError as exc:
            sys.stderr.write(f"error: {exc}\n")
            return 1
    emit_bail(
        gr_id,
        args.bail_class,
        args.bail_detail,
        child_key=_resolve_child_key(args.child_key),
    )
    return 0


if __name__ == "__main__":
    sys.exit(bail_main())
