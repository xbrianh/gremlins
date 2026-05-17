from __future__ import annotations

import argparse
import sys

from gremlins.executor.state import validate_gremlin_id
from gremlins.launcher import resume


def resume_main(argv: list[str]) -> int:
    p = argparse.ArgumentParser(
        prog="gremlins resume",
        description="Re-spawn an existing gremlin from its recorded stage.",
    )
    p.add_argument("gremlin_id")
    p.add_argument(
        "--graft",
        metavar="PIPELINE",
        help="Append this pipeline's stages (as a graft-N sequence) and resume into them.",
    )
    args = p.parse_args(argv)

    try:
        validate_gremlin_id(args.gremlin_id)
        resume(args.gremlin_id, graft=args.graft or None)
    except (ValueError, RuntimeError) as exc:
        sys.stderr.write(f"error: {exc}\n")
        return 1

    sys.stdout.write(f"resumed gremlin: {args.gremlin_id}\n")
    return 0
