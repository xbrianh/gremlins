from __future__ import annotations

import argparse
import pathlib
import sys
import time
from typing import Any

from _gremlins_core.config import get_config as _get_config
from _gremlins_core.config import state_root as _state_root_fn
from _gremlins_core.discovery import list_pipelines, resolve_pipeline_name
from _gremlins_core.schemas import Pipeline

from gremlins.launcher import launch
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
        "telemetry",
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
        "telemetry",
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
        "--telemetry",
        "-v",
        action="store_true",
        help="Enable per-turn telemetry (TTFT, token counts, cache hit ratio) in the gremlin log.",
    )
    source = None if pipeline is None else pipeline.bootstrap.source
    if source is not None:
        seen: set[str] = set()
        for key, src in source.sources.items():
            if key in seen:
                raise ValueError(
                    f"pipeline inputs produce duplicate flag --{key.replace('_', '-')}"
                )
            seen.add(key)
            flag = "--" + key.replace("_", "-")
            if flag.lstrip("-") in _INFRA_FLAG_NAMES:
                raise ValueError(
                    f"pipeline input {key!r} conflicts with infra flag {flag!r}"
                )
            kwargs: dict[str, Any] = {}
            if src.optional:
                kwargs["default"] = None
            else:
                kwargs["required"] = True
            p.add_argument(flag, dest=key, type=str, **kwargs)
    return p


def launch_main(argv: list[str]) -> int:
    if "--list" in argv:
        for name, path in list_pipelines():
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
        pipeline_path = resolve_pipeline_name(name)
    except FileNotFoundError as exc:
        sys.stderr.write(f"error: {exc}\n")
        return 1

    try:
        # Quick pre-parse of --client from argv so we can inline it into the
        # pipeline YAML before loader validation.  Fall back to the global
        # config's default-client when --client is absent.
        _client_override: str | None = None
        for i, a in enumerate(argv):
            if a == "--client" and i + 1 < len(argv):
                _client_override = argv[i + 1]
                break
            if a.startswith("--client="):
                _client_override = a.split("=", 1)[1]
                break
        if _client_override is None:
            _global_client = _get_config().default_client
            if _global_client:
                _client_override = _global_client
        pipeline = Pipeline.from_yaml(
            pipeline_path, default_client_override=_client_override
        )
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
    return _self_background_main(name, args, stage_inputs, telemetry=args.telemetry)


def _self_background_main(
    pipeline_name: str,
    args: argparse.Namespace,
    stage_inputs: dict[str, Any],
    *,
    telemetry: bool = False,
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
            telemetry=telemetry,
        )
    except (ValueError, RuntimeError) as exc:
        sys.stderr.write(f"error: {exc}\n")
        return 1

    state_root = pathlib.Path(_state_root_fn())
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
        info = (
            f"gremlin id:  {gremlin_id}\nlog:         {log_path}\nstate file:  {sf}\n"
        )
        sys.stderr.write(info)
        if args.print_id:
            sys.stdout.write(gremlin_id + "\n")
    if args.wait:
        return proc.wait()
    return 0
