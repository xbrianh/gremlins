from __future__ import annotations

import argparse
import pathlib
import sys
from typing import Any

from gremlins import paths as _paths
from gremlins.launcher import launch
from gremlins.pipeline import Pipeline
from gremlins.pipeline.discovery import list_pipelines, resolve_pipeline_name
from gremlins.pipeline.loader import STAGE_TYPES
from gremlins.stages.base import Stage
from gremlins.utils.yaml_io import YamlLoadError

_INFRA_ARGS = frozenset(
    {
        "description",
        "parent_id",
        "print_id",
        "print_id_only",
        "base_ref",
        "client",
        "gremlin_id",
        "wait",
    }
)
_INFRA_FLAG_NAMES = frozenset(
    {
        "description",
        "parent",
        "print-id",
        "print-id-only",
        "base-ref",
        "client",
        "gremlin-id",
        "wait",
    }
)
_LAUNCH_BRIEF = "usage: gremlins launch <name> [opts]\nLaunch a background gremlin by pipeline name. Run 'gremlins launch --list' to see available pipelines.\n"


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
    p.add_argument(
        "--gremlin-id",
        default=None,
        metavar="ID",
        help="Use a specific gremlin id (must match [a-z0-9-]+). Raises if a live gremlin with this id already exists.",
    )
    p.add_argument("--parent", dest="parent_id", default=None)
    p.add_argument("--print-id", action="store_true")
    p.add_argument(
        "--print-id-only",
        action="store_true",
        help="Print only the gremlin id on stdout; suppress the launch banner. Supersedes --print-id.",
    )
    p.add_argument(
        "--wait",
        action="store_true",
        help="Block until the spawned gremlin exits; return its exit code. No timeout — a hung gremlin blocks indefinitely.",
    )
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


def launch_main(argv: list[str]) -> int:
    if "--list" in argv:
        for name, path in list_pipelines(pathlib.Path.cwd()):
            try:
                pipeline = Pipeline.from_yaml(path)
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
        pipeline = Pipeline.from_yaml(pipeline_path)
    except (ValueError, YamlLoadError, FileNotFoundError) as exc:
        sys.stderr.write(
            f"error: pipeline '{name}' is invalid: {exc}\n  (file: {pipeline_path})\n"
        )
        return 1

    first = next((s for s in pipeline.stages if s.type != "parallel"), None)
    try:
        stage_cls = STAGE_TYPES[first.type] if first is not None else Stage
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
        gremlin_id, proc = launch(
            pipeline_name,
            stage_inputs=stage_inputs,
            description=args.description,
            parent_id=args.parent_id,
            base_ref=args.base_ref,
            pipeline_args=pipeline_args,
            gremlin_id=args.gremlin_id,
        )
    except (ValueError, RuntimeError) as exc:
        sys.stderr.write(f"error: {exc}\n")
        return 1

    state_root = _paths.state_root()
    state_dir = state_root / gremlin_id
    log_path = state_dir / "log"
    sf = state_dir / "state.json"

    if args.print_id_only:
        sys.stdout.write(gremlin_id + "\n")
    else:
        info = (
            f"gremlin id:  {gremlin_id}\nlog:         {log_path}\nstate file:  {sf}\n"
        )
        sys.stderr.write(info)
        if args.print_id:
            sys.stdout.write(gremlin_id + "\n")
    if args.wait:
        return proc.wait()
    return 0
