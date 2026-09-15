"""Internal spawn boundary: run a pipeline by path and record terminal state.

Usage: python -m gremlins.spawn.pipeline <gremlin_id> <pipeline_path> [args...]

Not intended for direct human invocation.
"""

from __future__ import annotations

import asyncio
import logging
import pathlib
import sys
import traceback

logger = logging.getLogger(__name__)


def main(argv: list[str] | None = None) -> int:
    from _gremlins_core.config import init as _init_config

    from gremlins.executor.gremlin import validate_gremlin_id, write_terminal_state
    from gremlins.logging_setup import configure_logging

    try:
        _init_config()
    except ValueError as exc:
        sys.stderr.write(f"run_pipeline: failed to load config: {exc}\n")
        return 1

    configure_logging()

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
        if rc != 0:
            logger.warning("pipeline finished with exit code %d", rc)
    except SystemExit as e:
        rc = e.code if isinstance(e.code, int) else 1
        logger.info("pipeline exited with code %d", rc)
    except BaseException:
        rc = 1
        logger.exception("pipeline terminated by exception")
        traceback.print_exc()
    finally:
        logger.info("writing terminal state (exit_code=%d)", rc)
        for h in logging.getLogger().handlers:
            try:
                h.flush()
            except Exception:
                pass
        write_terminal_state(gremlin_id, rc)
    sys.exit(rc)


if __name__ == "__main__":
    sys.exit(main())
