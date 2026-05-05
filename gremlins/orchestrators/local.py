"""Orchestrator entry points for the local pipeline."""

from __future__ import annotations

import argparse
import logging
import os
import pathlib
import shutil
import subprocess
import sys
from collections.abc import Callable
from typing import NoReturn

import yaml

from gremlins.clients import ClientSpec, to_client
from gremlins.clients.protocol import ClaudeClient
from gremlins.clients.resolve import (
    PACKAGE_DEFAULT,
    collect_stage_specs,
    load_stage_specs_from_state,
    require_stage_spec,
    validate_stage_specs,
)
from gremlins.env_file import load_env_file
from gremlins.git import in_git_repo
from gremlins.logging_setup import configure_logging
from gremlins.pipeline import (
    StageEntry,
    load_pipeline,
    resolve_pipeline_path,
)
from gremlins.prompts import load_prompts
from gremlins.runner import install_signal_handlers, make_parallel_wrapper, run_stages
from gremlins.stages import address_code, implement, plan, review_code, verify
from gremlins.stages.context import StageContext
from gremlins.state import patch_state, resolve_session_dir, set_stage

logger = logging.getLogger(__name__)

_CODE_STYLE_PATH = (
    pathlib.Path(__file__).resolve().parent.parent
    / "pipelines"
    / "prompts"
    / "code_style.md"
)

_DEFAULT_LENS = (
    pathlib.Path(__file__).resolve().parent.parent
    / "pipelines"
    / "prompts"
    / "review"
    / "detail.md"
)


def die(msg: str) -> NoReturn:
    sys.stderr.write(f"error: {msg}\n")
    sys.stderr.flush()
    sys.exit(1)


def _parse_local_args(argv: list[str]) -> argparse.Namespace:
    usage = (
        "usage: gremlins.cli local [--resume-from <stage>] [--plan <path>] [--spec <path>] "
        '[--cmd "<command>"] [--test-max-attempts <n>] '
        "[--pipeline <name-or-path>] [--client <provider:model>] "
        '"<instructions>"'
    )
    parser = argparse.ArgumentParser(add_help=False, usage=usage)
    parser.add_argument("--resume-from", dest="resume_from", default=None)
    parser.add_argument("--plan", dest="plan_path", default=None)
    parser.add_argument("--spec", dest="spec_path", default=None)
    parser.add_argument("--cmd", dest="cmds", action="append", default=None)
    parser.add_argument(
        "--test-max-attempts", dest="test_max_attempts", type=int, default=3
    )
    parser.add_argument("--pipeline", dest="pipeline", default=None)
    parser.add_argument("--client", dest="client", default=None)
    parser.add_argument("instructions", nargs="*")
    args = parser.parse_args(argv)
    if args.resume_from:
        args.instructions = [s for s in args.instructions if s]
    if args.plan_path:
        if args.instructions:
            die("--plan and positional instructions are mutually exclusive")
    else:
        if not args.instructions:
            die(usage)
    if args.test_max_attempts <= 0:
        die("--test-max-attempts must be a positive integer")
    if args.cmds is not None:
        for c in args.cmds:
            if not c.strip():
                die("--cmd: command must be a non-empty string")
    return args


def _build_stage_runner(
    entry: StageEntry,
    ctx: StageContext,
    model: str,
    *,
    args: argparse.Namespace,
    plan_file: pathlib.Path,
    spec_file: pathlib.Path,
    is_git: bool,
    code_style: str,
    instructions: str,
    plan_copied_from_source: bool,
    plan_text_holder: dict[str, str],
) -> Callable[[], None]:
    if entry.type == "plan":

        def _plan() -> None:
            if args.plan_path:
                if plan_copied_from_source:
                    logger.info("plan supplied via --plan (copied) -> %s", plan_file)
                else:
                    logger.info("plan reused from snapshot -> %s", plan_file)
            else:
                set_stage(ctx.gr_id, entry.name)
                logger.info("planning (model: %s) -> %s", model, plan_file)
                if not entry.prompt_paths:
                    die(
                        f"stage {entry.name!r}: type 'plan' requires a 'prompt' field in the pipeline YAML"
                    )
                plan.run(
                    ctx,
                    plan.PlanOptions(
                        plan_model=model,
                        plan_file=plan_file,
                        instructions=instructions,
                        code_style=code_style,
                        prompt_path=entry.prompt_paths[-1],
                    ),
                )

        return _plan

    if entry.type == "implement":

        def _implement() -> None:
            plan_text = plan_file.read_text(encoding="utf-8")
            plan_text_holder["text"] = plan_text
            spec_text = ""
            if spec_file.exists():
                try:
                    spec_text = spec_file.read_text(encoding="utf-8")
                except (OSError, UnicodeDecodeError) as exc:
                    logger.warning(
                        "could not read spec.md (%s); proceeding without north-star context",
                        exc,
                    )
            set_stage(ctx.gr_id, entry.name)
            logger.info("implementing (model: %s, from %s)", model, plan_file)
            implement.run(
                ctx,
                implement.ImplementOptions(
                    impl_model=model,
                    plan_text=plan_text,
                    code_style=code_style,
                    is_git=is_git,
                    spec_text=spec_text,
                    prompt_path=entry.prompt_paths[-1] if entry.prompt_paths else None,
                ),
            )

        return _implement

    if entry.type == "review-code":

        def _review_code() -> None:
            plan_text = plan_text_holder.get("text") or plan_file.read_text(
                encoding="utf-8"
            )
            set_stage(ctx.gr_id, entry.name)
            logger.info("reviewing code (model: %s)", model)
            review_file = review_code.run(
                ctx,
                review_code.ReviewCodeOptions(
                    plan_text=plan_text,
                    is_git=is_git,
                    code_style=code_style,
                    model=model,
                    stage_name=entry.name,
                    prompt_paths=entry.prompt_paths[1:]
                    if entry.prompt_paths
                    else [_DEFAULT_LENS],
                ),
            )
            logger.info("code review (%s): %s", model, review_file)

        return _review_code

    if entry.type == "address-code":

        def _address_code() -> None:
            set_stage(ctx.gr_id, entry.name)
            logger.info("addressing code reviews (model: %s)", model)
            opts = address_code.AddressCodeOptions(
                address_model=model,
                is_git=is_git,
                code_style=code_style,
            )
            if entry.prompt_paths:
                opts.prompt_path = entry.prompt_paths[-1]
            address_code.run(ctx, opts)

        return _address_code

    if entry.type == "verify":
        cmds = args.cmds if args.cmds is not None else entry.options.get("cmds", [])
        max_attempts = entry.options.get("max_attempts", args.test_max_attempts)

        def _verify() -> None:
            if cmds:
                set_stage(ctx.gr_id, entry.name)
                logger.info(
                    "running verify (cmds: %r, max-attempts: %s, model: %s)",
                    cmds,
                    max_attempts,
                    model,
                )
            verify.run(
                ctx,
                verify.VerifyOptions(
                    fix_model=model,
                    cwd=pathlib.Path.cwd(),
                    code_style=code_style,
                    is_git=is_git,
                    commit_after_fix=is_git,
                    cmds=cmds,
                    max_attempts=max_attempts,
                ),
            )

        return _verify

    raise ValueError(f"unsupported stage type {entry.type!r} in local pipeline")


def local_main(
    argv: list[str], *, client: ClaudeClient | None = None, gr_id: str | None = None
) -> int:
    configure_logging()
    args = _parse_local_args(argv)

    cli_spec: ClientSpec | None = None
    if args.client:
        try:
            cli_spec = ClientSpec.parse(args.client)
        except ValueError as exc:
            die(str(exc))

    if os.environ.get("GREMLINS_TEST_NOOP_PIPELINE"):
        return 0

    env_file = pathlib.Path(".gremlins/env")
    if env_file.is_file():
        try:
            os.environ.update(load_env_file(env_file))
        except RuntimeError as exc:
            die(str(exc))

    instructions = " ".join(args.instructions)

    if shutil.which("claude") is None:
        die("claude CLI not found")

    try:
        pipeline = load_pipeline(
            resolve_pipeline_path(args.pipeline or "local", pathlib.Path.cwd())
        )
    except (FileNotFoundError, ValueError, yaml.YAMLError) as exc:
        die(str(exc))

    # Load or resolve stage specs; state.json is authoritative on resume
    stage_specs: dict[str, ClientSpec] = {}
    if args.resume_from and gr_id:
        try:
            stage_specs = load_stage_specs_from_state(gr_id)
        except Exception as exc:
            die(f"--resume-from: corrupt state.json stage_clients: {exc}")
        if not stage_specs:
            die(
                "--resume-from: stage_clients not found in state.json (rerun from scratch?)"
            )
    if not stage_specs:
        stage_specs = collect_stage_specs(pipeline, cli_spec)
        if gr_id:
            patch_state(
                gr_id, stage_clients={k: str(v) for k, v in stage_specs.items()}
            )

    # Create one client instance per unique spec (or reuse injected test client)
    _spec_clients: dict[str, ClaudeClient] = {}

    def _client_for_spec(spec: ClientSpec) -> ClaudeClient:
        if client is not None:
            return client
        key = str(spec)
        if key not in _spec_clients:
            _spec_clients[key] = to_client(spec)
        return _spec_clients[key]

    for spec in stage_specs.values():
        _client_for_spec(spec)

    default_spec = cli_spec or pipeline.default_client or PACKAGE_DEFAULT
    if client is None:
        _client_for_spec(default_spec)

    if client is not None:
        install_signal_handlers(client)
    elif _spec_clients:
        all_clients = list(_spec_clients.values())
        install_signal_handlers(all_clients[0], *all_clients[1:])
    else:
        install_signal_handlers(to_client(default_spec))

    stage_names = [s.name for s in pipeline.stages]

    _child_to_group: dict[str, str] = {}
    for _e in pipeline.stages:
        if _e.type == "parallel":
            for _child in _e.children:
                if _child.name in _child_to_group or _child.name in stage_names:
                    die(f"duplicate child stage name {_child.name!r}")
                _child_to_group[_child.name] = _e.name

    all_valid_stages = stage_names + list(_child_to_group)
    seen: set[str] = set()
    for _n in all_valid_stages:
        if _n in seen:
            die(f"pipeline has duplicate stage name {_n!r}")
        seen.add(_n)

    if args.resume_from and args.resume_from not in all_valid_stages:
        die(
            f"--resume-from {args.resume_from!r} is not a valid stage; "
            f"valid: {all_valid_stages}"
        )

    run_resume_from = args.resume_from
    if args.resume_from in _child_to_group:
        run_resume_from = _child_to_group[args.resume_from]

    try:
        validate_stage_specs(stage_specs, pipeline)
    except ValueError as exc:
        die(str(exc))

    session_dir = resolve_session_dir(gr_id)
    plan_file = session_dir / "plan.md"

    # Determine the review-code output file path for the resume guard
    _rc_entry = next((s for s in pipeline.stages if s.type == "review-code"), None)
    if _rc_entry:
        _rc_model = require_stage_spec(stage_specs, _rc_entry.name).model
        review_code_file = session_dir / f"{_rc_entry.name}-{_rc_model}.md"
    else:
        review_code_file = session_dir / f"review-code-{PACKAGE_DEFAULT.model}.md"

    logger.info("session: %s", session_dir)

    plan_copied_from_source = False
    if args.plan_path and not plan_file.exists():
        src = pathlib.Path(args.plan_path)
        if not src.is_file():
            die(f"--plan: file not found: {args.plan_path}")
        if src.stat().st_size == 0:
            die(f"--plan: file is empty: {args.plan_path}")
        shutil.copyfile(src, plan_file)
        plan_copied_from_source = True

    spec_file = session_dir / "spec.md"
    if args.spec_path and not spec_file.exists():
        spec_src = pathlib.Path(args.spec_path)
        if not spec_src.is_file():
            die(f"--spec: file not found: {args.spec_path}")
        if spec_src.stat().st_size == 0:
            die(f"--spec: file is empty: {args.spec_path}")
        shutil.copyfile(spec_src, spec_file)

    is_git = in_git_repo()
    try:
        code_style = load_prompts([_CODE_STYLE_PATH])
    except (FileNotFoundError, ValueError) as exc:
        die(f"error loading prompt: {exc}")

    def _type_idx(stage_type: str) -> int:
        for i, s in enumerate(pipeline.stages):
            if s.type == stage_type:
                return i
        return len(pipeline.stages)

    start_idx = 0
    if run_resume_from:
        start_idx = stage_names.index(run_resume_from)
        if start_idx >= _type_idx("implement"):
            if not plan_file.exists() or plan_file.stat().st_size == 0:
                die(f"--resume-from {args.resume_from} requires existing {plan_file}")
        if start_idx >= _type_idx("review-code"):
            if is_git:
                porcelain = subprocess.run(
                    ["git", "status", "--porcelain"],
                    capture_output=True,
                    text=True,
                    check=False,
                )
                has_dirty = bool(porcelain.stdout.strip())
                r = subprocess.run(
                    ["git", "rev-list", "--count", "HEAD"],
                    capture_output=True,
                    text=True,
                    check=False,
                )
                has_commits = r.returncode == 0 and int(r.stdout.strip() or "0") > 0
                if not has_dirty and not has_commits:
                    die(
                        f"--resume-from {args.resume_from} requires implementation changes in the worktree"
                    )
            else:
                has_files = False
                for dirpath, dirnames, filenames in os.walk("."):
                    dirnames[:] = [d for d in dirnames if d != ".git"]
                    try:
                        sd_res = session_dir.resolve()
                        if pathlib.Path(dirpath).resolve() == sd_res:
                            dirnames[:] = []
                            continue
                    except Exception:
                        pass
                    if filenames:
                        has_files = True
                        break
                if not has_files:
                    die(
                        f"--resume-from {args.resume_from} requires implementation changes in the worktree"
                    )
        if start_idx >= _type_idx("address-code"):
            if not review_code_file.exists() or review_code_file.stat().st_size == 0:
                die(
                    f"--resume-from {args.resume_from} requires existing {review_code_file}"
                )

    plan_text_holder: dict[str, str] = {}

    stages: list[tuple[str, Callable[[], None]]] = []
    for e in pipeline.stages:
        if e.type == "parallel":
            group_dir = session_dir / e.name
            group_dir.mkdir(parents=True, exist_ok=True)
            child_runners: list[tuple[str, Callable[[], None]]] = []
            for child in e.children:
                child_spec = require_stage_spec(stage_specs, child.name)
                child_dir = group_dir / child.name
                child_dir.mkdir(parents=True, exist_ok=True)
                child_ctx = StageContext(
                    client=_client_for_spec(child_spec),
                    session_dir=child_dir,
                    gr_id=gr_id,
                )
                child_runners.append(
                    (
                        child.name,
                        _build_stage_runner(
                            child,
                            child_ctx,
                            child_spec.model,
                            args=args,
                            plan_file=plan_file,
                            spec_file=spec_file,
                            is_git=is_git,
                            code_style=code_style,
                            instructions=instructions,
                            plan_copied_from_source=plan_copied_from_source,
                            plan_text_holder=plan_text_holder,
                        ),
                    )
                )
            group_name = e.name
            stages.append(
                (
                    e.name,
                    make_parallel_wrapper(
                        child_runners,
                        max_concurrent=e.max_concurrent,
                        resume_from=args.resume_from,
                        set_stage_fn=lambda n=group_name: set_stage(gr_id, n),
                    ),
                )
            )
        else:
            stage_spec = require_stage_spec(stage_specs, e.name)
            stage_ctx = StageContext(
                client=_client_for_spec(stage_spec),
                session_dir=session_dir,
                gr_id=gr_id,
            )
            stages.append(
                (
                    e.name,
                    _build_stage_runner(
                        e,
                        stage_ctx,
                        stage_spec.model,
                        args=args,
                        plan_file=plan_file,
                        spec_file=spec_file,
                        is_git=is_git,
                        code_style=code_style,
                        instructions=instructions,
                        plan_copied_from_source=plan_copied_from_source,
                        plan_text_holder=plan_text_holder,
                    ),
                )
            )
    run_stages(stages, resume_from=run_resume_from)

    # Accumulate cost from all client instances
    total_cost = 0.0
    for c in _spec_clients.values() if _spec_clients else [client] if client else []:
        total_cost += getattr(c, "total_cost_usd", 0.0) or 0.0
    if total_cost > 0:
        patch_state(gr_id, total_cost_usd=total_cost)

    logger.info("done. session artifacts in: %s", session_dir)
    if total_cost > 0:
        logger.info("total cost: $%.4f", total_cost)
    return 0


def _parse_review_args(argv: list[str]) -> argparse.Namespace:
    usage = (
        "usage: gremlins.cli review [--dir <path>] [--plan <path>] [-b <detail-model>]"
    )
    parser = argparse.ArgumentParser(add_help=False, usage=usage)
    parser.add_argument("--dir", dest="dir", default=".")
    parser.add_argument("--plan", dest="plan", default=None)
    parser.add_argument("-b", dest="detail", default=PACKAGE_DEFAULT.model)
    args = parser.parse_args(argv)
    return args


def review_main(argv: list[str], *, client: ClaudeClient | None = None) -> int:
    configure_logging()
    from gremlins.clients.claude import SubprocessClaudeClient

    if client is None:
        client = SubprocessClaudeClient()
    install_signal_handlers(client)
    args = _parse_review_args(argv)

    if shutil.which("claude") is None:
        die("claude CLI not found")

    session_dir = pathlib.Path(args.dir).resolve()
    if not session_dir.is_dir():
        die(f"--dir does not exist: {session_dir}")

    plan_text = ""
    if args.plan is not None:
        plan_path = pathlib.Path(args.plan)
        if not plan_path.exists():
            die(f"--plan does not exist: {plan_path}")
        if not plan_path.is_file():
            die(f"--plan is not a file: {plan_path}")
        if plan_path.stat().st_size == 0:
            die(f"--plan is empty: {plan_path}")
        try:
            plan_text = plan_path.read_text(encoding="utf-8")
        except UnicodeDecodeError:
            die(f"--plan is not valid UTF-8 text: {plan_path}")
        except OSError as exc:
            die(f"failed to read --plan {plan_path}: {exc}")

    is_git = in_git_repo()
    try:
        code_style = load_prompts([_CODE_STYLE_PATH])
    except (FileNotFoundError, ValueError) as exc:
        die(f"error loading prompt: {exc}")
    if is_git:
        head1_exists = (
            subprocess.run(
                ["git", "rev-parse", "--verify", "HEAD~1"],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                check=False,
            ).returncode
            == 0
        )
        has_commit_diff = False
        if head1_exists:
            has_commit_diff = (
                subprocess.run(
                    ["git", "diff", "--quiet", "HEAD~1", "HEAD"],
                    check=False,
                ).returncode
                != 0
            )
        has_dirty = bool(
            subprocess.run(
                ["git", "status", "--porcelain"],
                capture_output=True,
                text=True,
                check=False,
            ).stdout.strip()
        )
        if not has_commit_diff and not has_dirty:
            if not head1_exists:
                die(
                    "nothing to review: no commit history beyond HEAD and working tree is clean"
                )
            die(
                "nothing to review: HEAD~1..HEAD has no changes and working tree is clean"
            )

    ctx = StageContext(client=client, session_dir=session_dir, gr_id=None)
    logger.info("reviewing code (model: %s)", args.detail)
    review_file = review_code.run(
        ctx,
        review_code.ReviewCodeOptions(
            plan_text=plan_text,
            is_git=is_git,
            code_style=code_style,
            model=args.detail,
            stage_name="review-code",
            prompt_paths=[_DEFAULT_LENS],
        ),
    )
    logger.info("code review (%s): %s", args.detail, review_file)
    return 0


def _parse_address_args(argv: list[str]) -> argparse.Namespace:
    usage = "usage: gremlins.cli address [--dir <path>] [-x <address-model>]"
    parser = argparse.ArgumentParser(add_help=False, usage=usage)
    parser.add_argument("--dir", dest="dir", default=".")
    parser.add_argument("-x", dest="address", default=PACKAGE_DEFAULT.model)
    args = parser.parse_args(argv)
    return args


def address_main(argv: list[str], *, client: ClaudeClient | None = None) -> int:
    configure_logging()
    from gremlins.clients.claude import SubprocessClaudeClient

    if client is None:
        client = SubprocessClaudeClient()
    install_signal_handlers(client)
    args = _parse_address_args(argv)

    if shutil.which("claude") is None:
        die("claude CLI not found")

    session_dir = pathlib.Path(args.dir).resolve()
    if not session_dir.is_dir():
        die(f"--dir does not exist: {session_dir}")

    is_git = in_git_repo()
    try:
        code_style = load_prompts([_CODE_STYLE_PATH])
    except (FileNotFoundError, ValueError) as exc:
        die(f"error loading prompt: {exc}")

    ctx = StageContext(client=client, session_dir=session_dir, gr_id=None)
    logger.info("addressing code reviews (model: %s)", args.address)
    address_code.run(
        ctx,
        address_code.AddressCodeOptions(
            address_model=args.address,
            is_git=is_git,
            code_style=code_style,
        ),
    )
    return 0
