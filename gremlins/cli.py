"""Top-level dispatch for ``python -m gremlins.cli``.

The first positional argument selects the subcommand:

- ``local``           — launch a background localgremlin (self-backgrounding)
- ``review``          — review-code stage only (was ``localreview.py``)
- ``address``         — address-code stage only (was ``localaddress.py``)
- ``gh``              — launch a background ghgremlin (self-backgrounding)
- ``boss``            — launch a background bossgremlin (self-backgrounding)
- ``fleet``           — fleet-manager subcommands (status / stop / rescue / land /
                        close / rm / log)
- ``handoff``         — chain-step decision agent (next-plan / chain-done / bail)
- ``resume``          — re-spawn an existing gremlin from its recorded stage (replaces
                        launch.sh --resume)
- ``bail``            — mark the running gremlin as bailed (reads GR_ID from env)
- ``session-summary`` — SessionStart / UserPromptSubmit hook (replaces session-summary.sh)
- ``_run-pipeline``   — internal spawn boundary; not for direct human use
- ``_gh``             — internal pipeline entry point for ghgremlin; not for direct use
- ``_local``          — internal pipeline entry point for localgremlin; not for direct use
- ``_boss``           — internal pipeline entry point for bossgremlin; not for direct use

Remaining argv is forwarded to the chosen orchestrator entry point.
"""

from __future__ import annotations

import argparse
import os
import pathlib
import sys
import traceback

from .fleet import main as fleet_main
from .fleet.session_summary import main as _session_summary_main
from .handoff import main as handoff_main
from .launcher import MODEL_RE, launch, resume, write_terminal_state
from .orchestrators.gh import gh_main
from .orchestrators.local import address_main, local_main, review_main
from .state import (
    BAIL_CLASS_OTHER,
    BAIL_CLASS_REVIEWER_REQUESTED_CHANGES,
    BAIL_CLASS_SECRETS,
    BAIL_CLASS_SECURITY,
    emit_bail,
    validate_gr_id,
)


def main(argv: list[str] | None = None, *, gr_id: str | None = None) -> int:
    if argv is None:
        argv = sys.argv[1:]
    if not argv:
        sys.stderr.write(
            "usage: python -m gremlins.cli "
            "{local|review|address|gh|boss|fleet|handoff|resume|bail|session-summary}"
            " [args...]\n"
        )
        return 1
    sub = argv[0]
    rest = argv[1:]
    if sub == "local":
        return _self_background_main("localgremlin", rest)
    if sub == "_local":
        return local_main(rest, gr_id=gr_id)
    if sub == "review":
        return review_main(rest)
    if sub == "address":
        return address_main(rest)
    if sub == "gh":
        return _self_background_main("ghgremlin", rest)
    if sub == "_gh":
        return gh_main(rest, gr_id=gr_id)
    if sub == "boss":
        return _self_background_main("bossgremlin", rest)
    if sub == "_boss":
        from .orchestrators.boss import boss_main

        return boss_main(rest, gr_id=gr_id)
    if sub == "fleet":
        return fleet_main(rest)
    if sub == "handoff":
        return handoff_main(rest)
    if sub == "resume":
        return _resume_main(rest)
    if sub == "bail":
        return _bail_main(rest)
    if sub == "session-summary":
        return _session_summary_main(rest)
    if sub == "_run-pipeline":
        return _run_pipeline_main(rest)
    sys.stderr.write(f"unknown subcommand: {sub}\n")
    return 1


def _validate_local_args(args: argparse.Namespace) -> None:
    if args.plan or args.instructions or args.positional_instructions:
        return
    raise ValueError(
        "localgremlin requires instructions: pass them as a positional argument, "
        "--plan <path>, or -c/--instructions <text>"
    )


def _validate_gh_args(rest: list[str]) -> None:
    p = argparse.ArgumentParser(add_help=False)
    p.add_argument("--model", default=None)
    args, _ = p.parse_known_args(rest)
    if args.model is not None and not MODEL_RE.match(args.model):
        raise ValueError(f"invalid model: {args.model!r}")


def _validate_boss_args(rest: list[str], plan: str | None) -> None:
    p = argparse.ArgumentParser(add_help=False)
    p.add_argument("--chain-kind", default=None)
    args, _ = p.parse_known_args(rest)
    if args.chain_kind not in ("local", "gh"):
        got = repr(args.chain_kind) if args.chain_kind is not None else "missing"
        raise ValueError(
            f"--chain-kind is required and must be 'local' or 'gh' ({got})"
        )
    if plan is None:
        raise ValueError("--plan is required")


def _self_background_main(kind: str, argv: list[str]) -> int:
    """Launch a background gremlin of the given kind and print its id/log/state."""
    p = argparse.ArgumentParser(
        prog=f"python -m gremlins.cli {kind.removesuffix('gremlin')}",
        description=f"Launch a background {kind}.",
    )
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
    p.add_argument("--spec", dest="spec_path", default=None)
    if kind == "localgremlin":
        p.add_argument("positional_instructions", nargs="?", default=None)
    else:
        # Keep the namespace attribute consistent so downstream code always sees it.
        p.set_defaults(positional_instructions=None)
    args, rest = p.parse_known_args(argv)

    try:
        if kind == "localgremlin":
            _validate_local_args(args)
        elif kind == "ghgremlin":
            _validate_gh_args(rest)
        elif kind == "bossgremlin":
            _validate_boss_args(rest, args.plan)
        instructions = args.instructions or args.positional_instructions
        gr_id = launch(
            kind,
            instructions=instructions,
            plan=args.plan,
            description=args.description,
            parent_id=args.parent_id,
            base_ref=args.base_ref,
            spec_path=args.spec_path,
            pipeline_args=tuple(rest),
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
        sys.stderr.write(info)
    return 0


def _resume_main(argv: list[str]) -> int:
    """CLI front-end for launcher.resume()."""
    p = argparse.ArgumentParser(
        prog="python -m gremlins.cli resume",
        description="Re-spawn an existing gremlin from its recorded stage.",
    )
    p.add_argument("gr_id")
    args = p.parse_args(argv)

    try:
        validate_gr_id(args.gr_id)
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
    try:
        validate_gr_id(gr_id)
    except ValueError as exc:
        sys.stderr.write(f"_run-pipeline: {exc}\n")
        return 1
    rc = 1
    try:
        rc = main([kind_subcommand, *args], gr_id=gr_id)
    except SystemExit as e:
        rc = e.code if isinstance(e.code, int) else 1
    except BaseException:
        rc = 1
        traceback.print_exc()
    finally:
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

    gr_id = os.environ.get("GR_ID")
    if gr_id is not None:
        try:
            validate_gr_id(gr_id)
        except ValueError as exc:
            sys.stderr.write(f"error: {exc}\n")
            return 1
    emit_bail(gr_id, args.bail_class, args.bail_detail)
    return 0


def _get_state_root():
    return (
        pathlib.Path(
            os.environ.get("XDG_STATE_HOME")
            or os.path.join(os.path.expanduser("~"), ".local", "state")
        )
        / "claude-gremlins"
    )


if __name__ == "__main__":
    sys.exit(main())
