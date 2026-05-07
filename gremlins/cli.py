"""Top-level dispatch for ``python -m gremlins.cli``.

User-facing subcommands:
  launch    — launch a background gremlin (local|gh|boss)
  init      — scaffold .gremlins/ with editable copies of bundled pipelines
  review    — review-code stage only
  address   — address-code stage only
  resume    — re-spawn an existing gremlin from its recorded stage
  ack       — record that a bailed child's work landed externally
  skip      — abandon a bailed child's work
  stop      — send SIGTERM to a running gremlin
  rescue    — diagnose and resume a dead or stalled gremlin
  land      — land a finished gremlin onto the current branch
  rm        — delete a dead gremlin's state dir, worktree, and branch
  close     — mark a dead gremlin as closed
  log       — tail the gremlin's log file

Internal (launcher-spawned, hidden from help):
  _local, _gh, _boss

Bare invocation prints fleet status.
"""

from __future__ import annotations

import argparse
import pathlib
import sys
from typing import Any

from gremlins import paths as _paths
from gremlins.fleet import main as fleet_main
from gremlins.fleet.cli import (
    ack_main,
    close_main,
    land_main,
    log_main,
    rescue_main,
    rm_main,
    skip_main,
    stop_main,
)
from gremlins.launcher import launch, resume
from gremlins.orchestrators.gh import gh_main
from gremlins.orchestrators.local import local_main
from gremlins.orchestrators.review_address import address_main, review_main
from gremlins.pipeline import load_pipeline, resolve_pipeline_name
from gremlins.stages.introspect import build_launch_parser
from gremlins.stages.registry import STAGE_REGISTRY
from gremlins.state import validate_gr_id

# None → generic "no longer valid"; str → migration hint naming the new form
_REMOVED: dict[str, str | None] = {
    "fleet": None,
    "handoff": None,
    "bail": None,
    "session-summary": None,
    "_run-pipeline": None,
    "local": "gremlins launch local",
    "gh": "gremlins launch gh",
    "boss": "gremlins launch boss",
}


def main(argv: list[str] | None = None, *, gr_id: str | None = None) -> int:
    if argv is None:
        argv = sys.argv[1:]

    if argv and argv[0] in _REMOVED:
        redirect = _REMOVED[argv[0]]
        if redirect:
            sys.stderr.write(
                f"error: '{argv[0]}' is no longer a top-level subcommand;"
                f" use '{redirect}'\n"
            )
        else:
            sys.stderr.write(f"error: '{argv[0]}' is no longer a valid subcommand\n")
        return 1

    sub = argv[0] if argv else ""
    rest = argv[1:]

    if sub == "launch":
        return _launch_main(rest)
    if sub == "init":
        from gremlins.init import init_main

        return init_main(rest)
    if sub == "_local":
        return local_main(rest, gr_id=gr_id)
    if sub == "review":
        return review_main(rest)
    if sub == "address":
        return address_main(rest)
    if sub == "_gh":
        return gh_main(rest, gr_id=gr_id)
    if sub == "_boss":
        from gremlins.orchestrators.boss import boss_main

        return boss_main(rest, gr_id=gr_id)
    if sub == "resume":
        return _resume_main(rest)
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

    # No subcommand or unknown first arg → fleet status (id-prefix drill-in works here)
    return fleet_main(argv)


class _EmptyStage:
    def __init__(self) -> None:
        pass


_INFRA_ARGS = frozenset(
    {"description", "parent_id", "print_id", "base_ref", "client", "spec_path", "plan"}
)

_LAUNCH_BRIEF = "usage: gremlins launch <name> [opts]\nLaunch a background gremlin by pipeline name.\n"


def _launch_main(argv: list[str]) -> int:
    name_idx = next((i for i, a in enumerate(argv) if not a.startswith("-")), None)

    if name_idx is None:
        sys.stdout.write(_LAUNCH_BRIEF)
        return 0 if ("--help" in argv or "-h" in argv) else 1

    name = argv[name_idx]

    try:
        pipeline_path = resolve_pipeline_name(name, pathlib.Path.cwd())
        pipeline = load_pipeline(pipeline_path)
    except FileNotFoundError as exc:
        sys.stderr.write(f"error: {exc}\n")
        return 1

    first = next((s for s in pipeline.stages if s.type != "parallel"), None)
    try:
        parser = build_launch_parser(
            name, STAGE_REGISTRY[first.type] if first is not None else _EmptyStage
        )
    except (KeyError, TypeError):
        parser = build_launch_parser(name, _EmptyStage)

    argv_for_parser = [a for i, a in enumerate(argv) if i != name_idx]
    try:
        args = parser.parse_args(argv_for_parser)
    except SystemExit as exc:
        return exc.code if isinstance(exc.code, int) else 1

    stage_inputs = {k: v for k, v in vars(args).items() if k not in _INFRA_ARGS}
    return _self_background_main(name, args, stage_inputs)


def _self_background_main(
    pipeline_name: str, args: argparse.Namespace, stage_inputs: dict[str, Any]
) -> int:
    try:
        gr_id = launch(
            pipeline_name,
            stage_inputs=stage_inputs,
            plan=args.plan,
            description=args.description,
            parent_id=args.parent_id,
            base_ref=args.base_ref,
            spec_path=args.spec_path,
        )
    except (ValueError, RuntimeError) as exc:
        sys.stderr.write(f"error: {exc}\n")
        return 1

    state_root = _paths.state_root()
    state_dir = state_root / gr_id
    log_path = state_dir / "log"
    sf = state_dir / "state.json"

    info = f"gremlin id:  {gr_id}\nlog:         {log_path}\nstate file:  {sf}\n"
    if args.print_id:
        sys.stderr.write(info)
        sys.stdout.write(gr_id + "\n")
    else:
        sys.stderr.write(info)
    return 0


def _resume_main(argv: list[str]) -> int:
    p = argparse.ArgumentParser(
        prog="gremlins resume",
        description="Re-spawn an existing gremlin from its recorded stage.",
    )
    p.add_argument("gr_id")
    args = p.parse_args(argv)

    try:
        validate_gr_id(args.gr_id)
        resume(args.gr_id)
    except (ValueError, RuntimeError) as exc:
        sys.stderr.write(f"error: {exc}\n")
        return 1

    sys.stdout.write(f"resumed gremlin: {args.gr_id}\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
