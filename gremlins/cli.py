"""Top-level dispatch for ``python -m gremlins.cli``.

The first positional argument selects the subcommand:

- ``local``           — full plan → implement → review-code → address-code chain
- ``review``          — review-code stage only (was ``localreview.py``)
- ``address``         — address-code stage only (was ``localaddress.py``)
- ``gh``              — full gh-issue-driven pipeline (Phase 3)
- ``boss``            — chained serial workflow driven by a top-level spec (Phase 4)
- ``fleet``           — fleet-manager subcommands (status / stop / rescue / land /
                        close / rm / log)
- ``handoff``         — chain-step decision agent (next-plan / chain-done / bail)
- ``launch``          — launch a new background gremlin (replaces launch.sh forward path)
- ``resume``          — re-spawn an existing gremlin from its recorded stage (replaces
                        launch.sh --resume)
- ``bail``            — mark the running gremlin as bailed (reads GR_ID from env)
- ``session-summary`` — SessionStart / UserPromptSubmit hook (replaces session-summary.sh)
- ``_run-pipeline``   — internal spawn boundary; not for direct human use

Remaining argv is forwarded to the chosen orchestrator entry point.
"""

from __future__ import annotations

import sys
import traceback


def main(argv: list[str] | None = None) -> int:
    if argv is None:
        argv = sys.argv[1:]
    if not argv:
        sys.stderr.write(
            "usage: python -m gremlins.cli "
            "{local|review|address|gh|boss|fleet|handoff|launch|resume|bail|session-summary}"
            " [args...]\n"
        )
        return 1
    sub = argv[0]
    rest = argv[1:]
    if sub == "local":
        from .orchestrators.local import local_main

        return local_main(rest)
    if sub == "review":
        from .orchestrators.local import review_main

        return review_main(rest)
    if sub == "address":
        from .orchestrators.local import address_main

        return address_main(rest)
    if sub == "gh":
        from .orchestrators.gh import gh_main

        return gh_main(rest)
    if sub == "boss":
        from .orchestrators.boss import boss_main

        return boss_main(rest)
    if sub == "fleet":
        from .fleet import main as fleet_main

        return fleet_main(rest)
    if sub == "handoff":
        from .handoff import main as handoff_main

        return handoff_main(rest)
    if sub == "launch":
        return _launch_main(rest)
    if sub == "resume":
        return _resume_main(rest)
    if sub == "bail":
        return _bail_main(rest)
    if sub == "session-summary":
        from .fleet.session_summary import main as _session_summary_main

        return _session_summary_main(rest)
    if sub == "_run-pipeline":
        return _run_pipeline_main(rest)
    sys.stderr.write(f"unknown subcommand: {sub}\n")
    return 1


def _launch_main(argv: list[str]) -> int:
    """CLI front-end for launcher.launch()."""
    import argparse

    from .launcher import VALID_KINDS, launch

    p = argparse.ArgumentParser(
        prog="python -m gremlins.cli launch",
        description="Launch a background gremlin.",
    )
    p.add_argument("kind", choices=sorted(VALID_KINDS))
    p.add_argument("--plan", default=None)
    p.add_argument("--description", default=None)
    p.add_argument("--parent", dest="parent_id", default=None)
    p.add_argument("--print-id", action="store_true")
    p.add_argument(
        "--instructions",
        "-c",
        default=None,
        help="Instructions string (mutually exclusive with --plan).",
    )
    p.add_argument(
        "--base-ref",
        default="HEAD",
        help="Git ref to branch the worktree from (default: HEAD). "
        "Applies to local gremlins only; ignored for gh gremlins, "
        "which always anchor to origin/<default-branch>.",
    )
    args, rest = p.parse_known_args(argv)

    instructions = args.instructions
    pipeline_flags = list(rest)

    try:
        gr_id = launch(
            args.kind,
            instructions=instructions,
            plan=args.plan,
            description=args.description,
            parent_id=args.parent_id,
            base_ref=args.base_ref,
            pipeline_args=tuple(pipeline_flags),
        )
    except (ValueError, RuntimeError) as exc:
        sys.stderr.write(f"error: {exc}\n")
        return 1

    state_root = _get_state_root()
    state_dir = state_root / gr_id
    log_path = state_dir / "log"
    sf = state_dir / "state.json"

    info = f"gremlin id:  {gr_id}\nlog:         {log_path}\nstate file:  {sf}\n"
    if args.print_id:
        sys.stderr.write(info)
        sys.stdout.write(gr_id + "\n")
    else:
        sys.stdout.write(info)
    return 0


def _resume_main(argv: list[str]) -> int:
    """CLI front-end for launcher.resume()."""
    import argparse

    from .launcher import resume

    p = argparse.ArgumentParser(
        prog="python -m gremlins.cli resume",
        description="Re-spawn an existing gremlin from its recorded stage.",
    )
    p.add_argument("gr_id")
    args = p.parse_args(argv)

    try:
        resume(args.gr_id)
    except (ValueError, RuntimeError) as exc:
        sys.stderr.write(f"error: {exc}\n")
        return 1

    sys.stdout.write(f"resumed gremlin: {args.gr_id}\n")
    return 0


def _run_pipeline_main(argv: list[str]) -> int:
    """Internal spawn boundary: run a pipeline subcommand and record terminal state.

    Usage: _run-pipeline <gr_id> <kind_subcommand> [pipeline_args...]

    Not intended for direct human invocation.
    """
    if len(argv) < 2:
        sys.stderr.write("_run-pipeline: usage: <gr_id> <kind_subcommand> [args...]\n")
        return 1

    gr_id, kind_subcommand, *args = argv
    rc = 1
    try:
        rc = main([kind_subcommand, *args])
    except SystemExit as e:
        rc = e.code if isinstance(e.code, int) else 1
    except BaseException:
        rc = 1
        traceback.print_exc()
    finally:
        from .launcher import write_terminal_state

        write_terminal_state(gr_id, exit_code=rc)
    sys.exit(rc)


def _bail_main(argv: list[str]) -> int:
    """CLI front-end for state.emit_bail.

    Reads GR_ID from env (set by the launcher when spawning the
    pipeline). Returns 0 for valid invocations (including when GR_ID is
    unset — emit_bail no-ops); invalid CLI usage is handled by argparse
    and exits non-zero (typically 2). A typo like ``bail othr`` should
    surface as an error rather than be silently swallowed.
    """
    import argparse

    from .state import (
        BAIL_CLASS_OTHER,
        BAIL_CLASS_REVIEWER_REQUESTED_CHANGES,
        BAIL_CLASS_SECRETS,
        BAIL_CLASS_SECURITY,
        emit_bail,
    )

    valid = {
        BAIL_CLASS_REVIEWER_REQUESTED_CHANGES,
        BAIL_CLASS_SECURITY,
        BAIL_CLASS_SECRETS,
        BAIL_CLASS_OTHER,
    }
    p = argparse.ArgumentParser(
        prog="python -m gremlins.cli bail",
        description="Mark the running gremlin as bailed.",
    )
    p.add_argument("bail_class", choices=sorted(valid))
    p.add_argument("bail_detail", nargs="?", default="")
    args = p.parse_args(argv)

    emit_bail(args.bail_class, args.bail_detail)
    return 0


def _get_state_root():
    import os
    import pathlib

    return (
        pathlib.Path(
            os.environ.get("XDG_STATE_HOME")
            or os.path.join(os.path.expanduser("~"), ".local", "state")
        )
        / "claude-gremlins"
    )


if __name__ == "__main__":
    sys.exit(main())
