from __future__ import annotations

import argparse
import pathlib
import sys

from gremlins.init import (
    build_plan,
    bundled_pipeline_names,
    check_conflicts,
    cleanup_tmp,
    commit_writes,
    stage_writes,
    tmp_path,
    validate_selection,
)
from gremlins.utils.yaml import YamlLoadError


def _parse_args(argv: list[str]) -> argparse.Namespace:
    p = argparse.ArgumentParser(
        prog="gremlins init",
        description="Scaffold .gremlins/ with editable copies of bundled pipelines.",
    )
    p.add_argument(
        "--pipeline",
        action="append",
        dest="pipelines",
        metavar="NAME",
        help="Pipeline to scaffold (repeatable; default: all).",
    )
    p.add_argument("--force", action="store_true", help="Overwrite existing files.")
    p.add_argument(
        "--path",
        default=None,
        metavar="DIR",
        help="Scaffold under DIR/.gremlins/ (default: cwd).",
    )
    return p.parse_args(argv)


def init_main(argv: list[str]) -> int:
    args = _parse_args(argv)
    bundled = bundled_pipeline_names()
    selected = list(dict.fromkeys(args.pipelines or bundled))
    if rc := validate_selection(selected, bundled):
        return rc
    plan: list[tuple[pathlib.Path, bytes]] = []
    try:
        base = pathlib.Path(args.path) if args.path else pathlib.Path.cwd()
        plan = build_plan(selected, base)
        if rc := check_conflicts(plan, args.force):
            return rc
        staged = stage_writes(plan)
        try:
            commit_writes(staged, plan)
        except OSError:
            cleanup_tmp(staged)
            raise
    except (OSError, YamlLoadError, ValueError) as exc:
        sys.stderr.write(f"error: {str(exc).splitlines()[0]}\n")
        cleanup_tmp([tmp_path(dst) for dst, _ in plan])
        return 1
    return 0
