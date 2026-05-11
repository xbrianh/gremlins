from __future__ import annotations

import argparse
import pathlib
import sys

from gremlins.init import (
    _build_plan,
    _bundled_pipeline_names,
    _check_conflicts,
    _cleanup_tmp,
    _commit_writes,
    _stage_writes,
    _tmp_path,
    _validate_selection,
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
    p.add_argument("--path", default=None, metavar="DIR", help="Scaffold under DIR/.gremlins/ (default: cwd).")
    return p.parse_args(argv)


def init_main(argv: list[str]) -> int:
    args = _parse_args(argv)
    bundled = _bundled_pipeline_names()
    selected = list(dict.fromkeys(args.pipelines or bundled))
    if rc := _validate_selection(selected, bundled):
        return rc
    plan: list[tuple[pathlib.Path, bytes]] = []
    try:
        base = pathlib.Path(args.path) if args.path else pathlib.Path.cwd()
        plan = _build_plan(selected, base)
        if rc := _check_conflicts(plan, args.force):
            return rc
        staged = _stage_writes(plan)
        try:
            _commit_writes(staged, plan)
        except OSError:
            _cleanup_tmp(staged)
            raise
    except (OSError, YamlLoadError, ValueError) as exc:
        sys.stderr.write(f"error: {str(exc).splitlines()[0]}\n")
        _cleanup_tmp([_tmp_path(dst) for dst, _ in plan])
        return 1
    return 0
