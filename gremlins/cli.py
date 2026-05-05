"""Top-level dispatch for ``python -m gremlins.cli``.

User-facing subcommands:
  launch    — launch a background gremlin (local|gh|boss)
  init      — scaffold .gremlins/ with editable copies of bundled pipelines
  review    — review-code stage only
  address   — address-code stage only
  resume    — re-spawn an existing gremlin from its recorded stage
  ack       — record that a bailed child's work landed externally
  skip      — abandon a bailed child's work
  stop      — send SIGTERM to a running gremlin
  rescue    — diagnose and resume a dead or stalled gremlin
  land      — land a finished gremlin onto the current branch
  rm        — delete a dead gremlin's state dir, worktree, and branch
  close     — mark a dead gremlin as closed
  log       — tail the gremlin's log file

Internal (launcher-spawned, hidden from help):
  _local, _gh, _boss

Bare invocation prints fleet status.
"""

from __future__ import annotations

import argparse
import os
import pathlib
import sys

import yaml

from .fleet import main as fleet_main
from .fleet.cli import (
    ack_main,
    close_main,
    land_main,
    log_main,
    rescue_main,
    rm_main,
    skip_main,
    stop_main,
)
from .launcher import MODEL_RE, launch, resume
from .orchestrators.gh import gh_main
from .orchestrators.local import address_main, local_main, review_main
from .pipeline import load_pipeline, resolve_pipeline_path
from .state import validate_gr_id

# None → generic "no longer valid"; str → migration hint naming the new form
_REMOVED: dict[str, str | None] = {
    "fleet": None,
    "handoff": None,
    "bail": None,
    "session-summary": None,
    "_run-pipeline": None,
    "local": "gremlins launch local",
    "gh": "gremlins launch gh",
    "boss": "gremlins launch boss",
}


def main(argv: list[str] | None = None, *, gr_id: str | None = None) -> int:
    if argv is None:
        argv = sys.argv[1:]

    if argv and argv[0] in _REMOVED:
        redirect = _REMOVED[argv[0]]
        if redirect:
            sys.stderr.write(
                f"error: '{argv[0]}' is no longer a top-level subcommand;"
                f" use '{redirect}'\n"
            )
        else:
            sys.stderr.write(f"error: '{argv[0]}' is no longer a valid subcommand\n")
        return 1

    sub = argv[0] if argv else ""
    rest = argv[1:]

    if sub == "launch":
        return _launch_main(rest)
    if sub == "init":
        from .init import init_main

        return init_main(rest)
    if sub == "_local":
        return local_main(rest, gr_id=gr_id)
    if sub == "review":
        return review_main(rest)
    if sub == "address":
        return address_main(rest)
    if sub == "_gh":
        return gh_main(rest, gr_id=gr_id)
    if sub == "_boss":
        from .orchestrators.boss import boss_main

        return boss_main(rest, gr_id=gr_id)
    if sub == "resume":
        return _resume_main(rest)
    if sub == "stop":
        return stop_main(rest)
    if sub == "rescue":
        return rescue_main(rest)
    if sub == "land":
        return land_main(rest)
    if sub == "rm":
        return rm_main(rest)
    if sub == "close":
        return close_main(rest)
    if sub == "log":
        return log_main(rest)
    if sub == "ack":
        return ack_main(rest)
    if sub == "skip":
        return skip_main(rest)

    # No subcommand or unknown first arg → fleet status (id-prefix drill-in works here)
    return fleet_main(argv)


_LAUNCH_KINDS = {"local": "localgremlin", "gh": "ghgremlin", "boss": "bossgremlin"}

_LAUNCH_HELP = """\
usage: gremlins launch <kind> [opts]

Launch a background gremlin.

Kinds:
  local  Full local pipeline: plan → implement → review-code → address-code
  gh     GitHub issue-driven pipeline
  boss   Chained serial workflow

Flag reference: README.md § "Launch flags"
"""


def _launch_main(argv: list[str]) -> int:
    kind_name = argv[0] if argv and not argv[0].startswith("-") else None

    if kind_name in _LAUNCH_KINDS:
        return _self_background_main(_LAUNCH_KINDS[kind_name], argv[1:])

    if kind_name is not None:
        sys.stderr.write(
            f"error: unknown launch kind: {kind_name!r} (choose: local, gh, boss)\n"
        )
        return 1

    # No kind given: print help; exit 0 for --help, 1 for bare call
    sys.stdout.write(_LAUNCH_HELP)
    return 0 if ("--help" in argv or "-h" in argv) else 1


def _validate_local_args(args: argparse.Namespace) -> None:
    if args.plan or args.instructions or args.positional_instructions:
        return
    raise ValueError(
        "localgremlin requires instructions: pass them as a positional argument, "
        "--plan <path>, or -c/--instructions <text>"
    )


def _validate_gh_args(args: argparse.Namespace, rest: list[str]) -> None:
    p = argparse.ArgumentParser(add_help=False)
    p.add_argument("--model", default=None)
    p.add_argument("--resume-from", default=None)
    p.add_argument("--pipeline", default=None)
    try:
        parsed, remainder = p.parse_known_args(rest)
    except SystemExit as exc:
        raise ValueError(f"invalid gh arguments (exit {exc.code})") from exc

    positional = [t for t in remainder if not t.startswith("-")]
    if (
        args.plan is None
        and args.instructions is None
        and parsed.resume_from is None
        and not positional
    ):
        raise ValueError("instructions, --plan, or --resume-from required")
    if parsed.resume_from is not None:
        try:
            pipeline = load_pipeline(
                resolve_pipeline_path(parsed.pipeline or "gh", pathlib.Path.cwd())
            )
        except (FileNotFoundError, ValueError, yaml.YAMLError) as exc:
            raise ValueError(f"could not load pipeline for --resume-from check: {exc}")
        stage_names = [s.name for s in pipeline.stages]
        if parsed.resume_from not in stage_names:
            raise ValueError(
                f"invalid --resume-from: {parsed.resume_from!r} "
                f"(allowed: {' '.join(stage_names)})"
            )
    if parsed.model is not None and not MODEL_RE.match(parsed.model):
        raise ValueError(f"invalid model: {parsed.model!r}")


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
        prog=f"gremlins launch {kind.removesuffix('gremlin')}",
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
        p.set_defaults(positional_instructions=None)
    args, rest = p.parse_known_args(argv)

    try:
        if kind == "localgremlin":
            _validate_local_args(args)
        elif kind == "ghgremlin":
            _validate_gh_args(args, rest)
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
    p = argparse.ArgumentParser(
        prog="gremlins resume",
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
