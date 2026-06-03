"""Internal spawn boundary: run a pipeline by path and record terminal state.

Usage: python -m gremlins.spawn.pipeline <gremlin_id> <pipeline_path> [args...]

Not intended for direct human invocation.
"""

from __future__ import annotations

import asyncio
import pathlib
import sys
import traceback

from gremlins.executor.state import StateData
from gremlins.executor.state_utils import validate_gremlin_id


def main(argv: list[str] | None = None) -> int:
    if argv is None:
        argv = sys.argv[1:]
    if len(argv) < 2:
        sys.stderr.write(
            "run_pipeline: usage: <gremlin_id> <pipeline_path> [args...]\n"
        )
        return 1

    gremlin_id, pipeline_arg, *args = argv
    try:
        validate_gremlin_id(gremlin_id)
    except ValueError as exc:
        sys.stderr.write(f"run_pipeline: {exc}\n")
        return 1

    from gremlins.executor.run import run_pipeline as _run_pipeline

    rc = 1
    try:
        rc = asyncio.run(
            _run_pipeline(pathlib.Path(pipeline_arg), argv=args, gremlin_id=gremlin_id)
        )
    except SystemExit as e:
        rc = e.code if isinstance(e.code, int) else 1
    except BaseException:
        rc = 1
        traceback.print_exc()
    finally:
        StateData.load(gremlin_id).write_terminal_state(rc)
    sys.exit(rc)


if __name__ == "__main__":
    sys.exit(main())
