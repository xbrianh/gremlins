"""Internal spawn boundary: run a pipeline subcommand and record terminal state.

Usage: python -m gremlins.run_pipeline <gr_id> <kind_subcommand> [pipeline_args...]

Not intended for direct human invocation.
"""

from __future__ import annotations

import sys
import traceback

from .launcher import write_terminal_state
from .state import validate_gr_id


def main(argv: list[str] | None = None) -> int:
    if argv is None:
        argv = sys.argv[1:]
    if len(argv) < 2:
        sys.stderr.write("run_pipeline: usage: <gr_id> <kind_subcommand> [args...]\n")
        return 1

    gr_id, kind_subcommand, *args = argv
    try:
        validate_gr_id(gr_id)
    except ValueError as exc:
        sys.stderr.write(f"run_pipeline: {exc}\n")
        return 1

    from .cli import main as cli_main

    rc = 1
    try:
        rc = cli_main([kind_subcommand, *args], gr_id=gr_id)
    except SystemExit as e:
        rc = e.code if isinstance(e.code, int) else 1
    except BaseException:
        rc = 1
        traceback.print_exc()
    finally:
        write_terminal_state(gr_id, exit_code=rc)
    return rc


if __name__ == "__main__":
    sys.exit(main())
