from __future__ import annotations

import argparse
import importlib
import os
import pathlib
import sys
import time
from typing import Any

from gremlins import paths as _paths
from gremlins.clients.registry import CLIENT_FACTORIES
from gremlins.launcher import launch
from gremlins.permissions.loader import load_policy
from gremlins.permissions.validation import validate_policy_against_registry
from gremlins.pipeline import Pipeline
from gremlins.pipeline.discovery import list_pipelines, resolve_pipeline_name
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
        "bypass",
        "permissions_file",
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
        "bypass",
        "permissions-file",
    }
)
_LAUNCH_BRIEF = "usage: gremlins launch <name> [opts]\nLaunch a background gremlin by pipeline name. Run 'gremlins launch --list' to see available pipelines.\n"
_LOG_TAIL_BYTES = 4096


def build_launch_parser(
    pipeline_name: str, pipeline: Pipeline | None = None
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
    p.add_argument(
        "--bypass",
        action="store_true",
        default=False,
        help="Skip permission checks; run in bypass mode.",
    )
    p.add_argument(
        "--permissions-file",
        dest="permissions_file",
        type=pathlib.Path,
        default=None,
        metavar="PATH",
        help="Path to a permissions YAML file to load instead of the project default.",
    )
    if pipeline is not None and pipeline.inputs is not None:
        seen: set[str] = set()
        for path in pipeline.inputs.in_map.values():
            key, sep, default = path.partition("?")
            registry_key = key.split(".")[0]
            if registry_key in seen:
                raise ValueError(
                    f"pipeline inputs produce duplicate flag --{registry_key.replace('_', '-')}"
                )
            seen.add(registry_key)
            flag = "--" + registry_key.replace("_", "-")
            if flag.lstrip("-") in _INFRA_FLAG_NAMES:
                raise ValueError(
                    f"pipeline input {registry_key!r} conflicts with infra flag {flag!r}"
                )
            kwargs: dict[str, Any] = {}
            if sep:
                kwargs["default"] = default or None
            else:
                kwargs["required"] = True
            p.add_argument(flag, dest=registry_key, type=str, **kwargs)
    return p


def launch_main(argv: list[str]) -> int:
    if "--list" in argv:
        for name, path in list_pipelines(_paths.project_root()):
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
        pipeline_path = resolve_pipeline_name(name, _paths.project_root())
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

    parser = build_launch_parser(name, pipeline)

    try:
        args = parser.parse_args(argv[1:])
    except SystemExit as exc:
        return exc.code if isinstance(exc.code, int) else 1

    stage_inputs = {k: v for k, v in vars(args).items() if k not in _INFRA_ARGS}
    if stage_inputs.get("pr") and args.base_ref:
        sys.stderr.write("error: --pr and --base-ref are mutually exclusive\n")
        return 1
    return _self_background_main(name, args, stage_inputs)


def _self_background_main(
    pipeline_name: str, args: argparse.Namespace, stage_inputs: dict[str, Any]
) -> int:
    importlib.import_module("gremlins.clients")
    try:
        policy = load_policy(
            cli_bypass=args.bypass or None,
            cli_permissions_file=args.permissions_file,
            env=os.environ,
            cwd=_paths.project_root(),
        )
    except Exception as exc:
        sys.stderr.write(f"error: failed to load permissions policy: {exc}\n")
        return 1
    try:
        validate_policy_against_registry(policy, set(CLIENT_FACTORIES))
    except ValueError as exc:
        sys.stderr.write(f"error: {exc}\n")
        return 1

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
            bypass=policy.bypass,
            permissions_file=str(args.permissions_file.resolve())
            if args.permissions_file
            else "",
        )
    except (ValueError, RuntimeError) as exc:
        sys.stderr.write(f"error: {exc}\n")
        return 1

    state_root = _paths.state_root()
    state_dir = state_root / gremlin_id
    log_path = state_dir / "log"
    sf = state_dir / "state.json"

    deadline = time.time() + 2
    rc = proc.poll()
    while rc is None and time.time() < deadline:
        time.sleep(0.1)
        rc = proc.poll()
    if rc is not None:
        sys.stderr.write(f"error: gremlin {gremlin_id} exited early with code {rc}\n")
        if log_path.is_file():
            sys.stderr.write(
                log_path.read_bytes()[-_LOG_TAIL_BYTES:].decode(
                    "utf-8", errors="replace"
                )
            )
        return rc

    if args.print_id_only:
        sys.stdout.write(gremlin_id + "\n")
    else:
        perm_mode = "bypass" if policy.bypass else "default (allowlist)"
        info = (
            f"gremlin id:  {gremlin_id}\n"
            f"log:         {log_path}\n"
            f"state file:  {sf}\n"
            f"permissions: {perm_mode}\n"
        )
        sys.stderr.write(info)
        if args.print_id:
            sys.stdout.write(gremlin_id + "\n")
    if args.wait:
        return proc.wait()
    return 0
