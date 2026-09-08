"""gremlins validate — load and validate a pipeline YAML file."""

from __future__ import annotations

import argparse
import pathlib
import sys


def validate_main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(
        prog="gremlins validate",
        description="Load and validate a gremlin pipeline YAML file.",
    )
    parser.add_argument(
        "gremlin_def",
        metavar="<gremlin-def>",
        help="Path to a pipeline YAML file (e.g. ./.gremlins/gh.yaml)",
    )
    args = parser.parse_args(argv)

    path = pathlib.Path(args.gremlin_def).resolve()

    if not path.exists():
        sys.stderr.write(f"error: file not found: {path}\n")
        return 1

    if path.suffix not in (".yaml", ".yml"):
        sys.stderr.write(f"error: not a YAML file: {path}\n")
        return 1

    from _gremlins_core.schemas import Pipeline

    try:
        Pipeline.from_yaml(path)
    except Exception as exc:
        sys.stderr.write(f"error: {path}: {exc}\n")
        return 1

    sys.stderr.write(f"ok: {path}\n")
    return 0
