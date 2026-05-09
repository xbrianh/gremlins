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

Bare invocation prints fleet status.
"""

from __future__ import annotations

import argparse
import pathlib
import sys
from typing import Any, cast

import yaml

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
from gremlins.orchestrators.review_address import address_main, review_main
from gremlins.pipeline import list_pipelines, load_pipeline, resolve_pipeline_name
from gremlins.stages.base import Stage
from gremlins.stages.registry import STAGE_REGISTRY
from gremlins.state import validate_gr_id


def main(argv: list[str] | None = None) -> int:
    if argv is None:
        argv = sys.argv[1:]

    sub = argv[0] if argv else ""
    rest = argv[1:]

    if sub == "launch":
        return _launch_main(rest)
    if sub == "init":
        from gremlins.init import init_main

        return init_main(rest)
    if sub == "review":
        return review_main(rest)
    if sub == "address":
        return address_main(rest)
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


_INFRA_ARGS = frozenset({"description", "parent_id", "print_id", "base_ref", "client"})

_INFRA_FLAG_NAMES = frozenset(
    {"description", "parent", "print-id", "base-ref", "client"}
)


def _parse_bool(v: str) -> bool:
    if v.lower() in ("1", "true", "yes"):
        return True
    if v.lower() in ("0", "false", "no"):
        return False
    raise argparse.ArgumentTypeError(f"invalid bool value: {v!r}")


def build_launch_parser(
    pipeline_name: str, stage_cls: type[Stage]
) -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(prog=f"gremlins launch {pipeline_name}")
    p.add_argument("--description", default=None)
    p.add_argument("--parent", dest="parent_id", default=None)
    p.add_argument("--print-id", action="store_true")
    p.add_argument("--base-ref", default=None)
    p.add_argument("--client", default=None)
    for si in stage_cls.orchestration_args():
        flag = "--" + si.name.replace("_", "-")
        if flag.lstrip("-") in _INFRA_FLAG_NAMES:
            raise ValueError(
                f"stage input {si.name!r} conflicts with infra flag {flag!r}"
            )
        kwargs: dict[str, Any] = {"help": si.help}
        if si.type is bool:
            if si.required:
                kwargs["type"] = _parse_bool
                kwargs["required"] = True
            else:
                kwargs["action"] = argparse.BooleanOptionalAction
                kwargs["default"] = si.default
        else:
            kwargs["type"] = si.type
            if si.required:
                kwargs["required"] = True
            else:
                kwargs["default"] = si.default
        p.add_argument(flag, **kwargs)
    return p


_LAUNCH_BRIEF = "usage: gremlins launch <name> [opts]\nLaunch a background gremlin by pipeline name. Run 'gremlins launch --list' to see available pipelines.\n"


def _launch_main(argv: list[str]) -> int:
    if "--list" in argv:
        for name, path in list_pipelines(pathlib.Path.cwd()):
            try:
                pipeline = load_pipeline(path)
                label = pipeline.name
            except Exception:
                label = "unloadable"
            sys.stdout.write(f"{name}  {path.parent}  ({label})\n")
        return 0

    if not argv or argv[0].startswith("-"):
        sys.stdout.write(_LAUNCH_BRIEF)
        return 0 if ("--help" in argv or "-h" in argv) else 1

    name = argv[0]

    try:
        pipeline_path = resolve_pipeline_name(name, pathlib.Path.cwd())
    except FileNotFoundError as exc:
        sys.stderr.write(f"error: {exc}\n")
        return 1

    try:
        pipeline = load_pipeline(pipeline_path)
    except (ValueError, yaml.YAMLError, FileNotFoundError) as exc:
        sys.stderr.write(
            f"error: pipeline '{name}' is invalid: {exc}\n  (file: {pipeline_path})\n"
        )
        return 1

    first = next((s for s in pipeline.stages if s.type != "parallel"), None)
    try:
        stage_cls = (
            cast(type[Stage], STAGE_REGISTRY[first.type])
            if first is not None
            else Stage
        )
        parser = build_launch_parser(name, stage_cls)
    except (KeyError, TypeError):
        parser = build_launch_parser(name, Stage)

    try:
        args = parser.parse_args(argv[1:])
    except SystemExit as exc:
        return exc.code if isinstance(exc.code, int) else 1

    stage_inputs = {k: v for k, v in vars(args).items() if k not in _INFRA_ARGS}
    return _self_background_main(name, args, stage_inputs)


def _self_background_main(
    pipeline_name: str, args: argparse.Namespace, stage_inputs: dict[str, Any]
) -> int:
    pipeline_args = ("--client", args.client) if args.client else ()
    try:
        gr_id = launch(
            pipeline_name,
            stage_inputs=stage_inputs,
            description=args.description,
            parent_id=args.parent_id,
            base_ref=args.base_ref,
            pipeline_args=pipeline_args,
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
