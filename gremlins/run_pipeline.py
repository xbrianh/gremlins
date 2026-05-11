"""Internal spawn boundary: run a pipeline by path and record terminal state.

Usage: python -m gremlins.run_pipeline <gr_id> <pipeline_path> [args...]

Not intended for direct human invocation.
"""

from __future__ import annotations

import pathlib
import sys
import traceback

from gremlins.executor.state import validate_gr_id
from gremlins.launcher import write_terminal_state


def main(argv: list[str] | None = None) -> int:
    if argv is None:
        argv = sys.argv[1:]
    if len(argv) < 2:
        sys.stderr.write("run_pipeline: usage: <gr_id> <pipeline_path> [args...]\n")
        return 1

    gr_id, pipeline_arg, *args = argv
    try:
        validate_gr_id(gr_id)
    except ValueError as exc:
        sys.stderr.write(f"run_pipeline: {exc}\n")
        return 1

    from gremlins.orchestrators.run import run_pipeline as _run_pipeline

    rc = 1
    try:
        rc = _run_pipeline(pathlib.Path(pipeline_arg), argv=args, gr_id=gr_id)
    except SystemExit as e:
        rc = e.code if isinstance(e.code, int) else 1
    except BaseException:
        rc = 1
        traceback.print_exc()
    finally:
        write_terminal_state(gr_id, exit_code=rc)
    sys.exit(rc)


if __name__ == "__main__":
    sys.exit(main())
