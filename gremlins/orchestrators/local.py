"""Orchestrator entry points for the local pipeline."""

from __future__ import annotations

import argparse
import logging
import os
import pathlib
import re
import shutil
import subprocess
import sys
from typing import NoReturn

from ..clients.claude import ClaudeClient, SubprocessClaudeClient
from ..git import in_git_repo
from ..logging_setup import configure_logging
from ..prompts import BUNDLED_PROMPT_DIR, load_prompts
from ..runner import install_signal_handlers, run_stages
from ..stages import address_code, implement, plan, test
from ..stages.context import StageContext
from ..stages.review_code import run_review_code_stage
from ..state import patch_state, resolve_session_dir, set_stage

logger = logging.getLogger(__name__)

MODEL_RE = re.compile(r"^[A-Za-z0-9._-]+$")
VALID_RESUME_STAGES = ["plan", "implement", "review-code", "address-code", "test"]


def die(msg: str) -> NoReturn:
    sys.stderr.write(f"error: {msg}\n")
    sys.stderr.flush()
    sys.exit(1)


def _parse_local_args(argv: list[str]) -> argparse.Namespace:
    usage = (
        "usage: gremlins.cli local [-p <plan-model>] [-i <impl-model>] "
        "[-x <address-model>] [-b <detail-review-model>] "
        "[--resume-from <stage>] [--plan <path>] [--spec <path>] "
        '[--test "<command>"] [--test-max-attempts <n>] [-t <test-fix-model>] '
        '"<instructions>"'
    )
    parser = argparse.ArgumentParser(add_help=False, usage=usage)
    parser.add_argument("-p", dest="plan_model", default="sonnet")
    parser.add_argument("-i", dest="impl", default="sonnet")
    parser.add_argument("-x", dest="address", default="sonnet")
    parser.add_argument("-b", dest="detail", default="sonnet")
    parser.add_argument("-t", dest="test_fix_model", default="sonnet")
    parser.add_argument(
        "--resume-from", dest="resume_from", default=None, choices=VALID_RESUME_STAGES
    )
    parser.add_argument("--plan", dest="plan_path", default=None)
    parser.add_argument("--spec", dest="spec_path", default=None)
    parser.add_argument("--test", dest="test_cmd", default=None)
    parser.add_argument(
        "--test-max-attempts", dest="test_max_attempts", type=int, default=3
    )
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
    for m in (
        args.plan_model,
        args.impl,
        args.address,
        args.detail,
        args.test_fix_model,
    ):
        if not MODEL_RE.match(m):
            die(f"invalid model: {m}")
    if args.test_max_attempts <= 0:
        die("--test-max-attempts must be a positive integer")
    if args.test_cmd is not None and not args.test_cmd.strip():
        die("--test: command must be a non-empty string")
    return args


def local_main(argv: list[str], *, client: ClaudeClient | None = None) -> int:
    configure_logging()
    if client is None:
        client = SubprocessClaudeClient()
    install_signal_handlers(client)

    args = _parse_local_args(argv)
    if os.environ.get("GREMLINS_TEST_NOOP_PIPELINE"):
        return 0
    instructions = " ".join(args.instructions)

    if shutil.which("claude") is None:
        die("claude CLI not found")

    session_dir = resolve_session_dir()
    plan_file = session_dir / "plan.md"
    review_code_file = session_dir / f"review-code-detail-{args.detail}.md"

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
        code_style = load_prompts([BUNDLED_PROMPT_DIR / "code_style.md"])
    except (FileNotFoundError, ValueError) as exc:
        die(f"error loading prompt: {exc}")

    # Resume preconditions
    start_idx = 0
    if args.resume_from:
        start_idx = VALID_RESUME_STAGES.index(args.resume_from)
        if start_idx >= VALID_RESUME_STAGES.index("implement"):
            if not plan_file.exists() or plan_file.stat().st_size == 0:
                die(f"--resume-from {args.resume_from} requires existing {plan_file}")
        if start_idx >= VALID_RESUME_STAGES.index("review-code"):
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
        if start_idx >= VALID_RESUME_STAGES.index("address-code"):
            if not review_code_file.exists() or review_code_file.stat().st_size == 0:
                die(
                    f"--resume-from {args.resume_from} requires existing {review_code_file}"
                )

    ctx = StageContext(
        client=client,
        session_dir=session_dir,
        gr_id=os.environ.get("GR_ID"),
    )

    plan_text_holder: dict[str, str] = {}

    def stage_plan() -> None:
        if args.plan_path:
            if plan_copied_from_source:
                logger.info("[1/5] plan supplied via --plan (copied) -> %s", plan_file)
            else:
                logger.info("[1/5] plan reused from snapshot -> %s", plan_file)
        else:
            set_stage("plan")
            logger.info("[1/5] planning (model: %s) -> %s", args.plan_model, plan_file)
            plan.run(
                ctx,
                plan.PlanOptions(
                    plan_model=args.plan_model,
                    plan_file=plan_file,
                    instructions=instructions,
                    code_style=code_style,
                ),
            )

    def stage_implement() -> None:
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
        set_stage("implement")
        logger.info("[2/5] implementing (model: %s, from %s)", args.impl, plan_file)
        implement.run(
            ctx,
            implement.ImplementOptions(
                impl_model=args.impl,
                plan_text=plan_text,
                code_style=code_style,
                is_git=is_git,
                spec_text=spec_text,
            ),
        )

    def stage_review_code() -> None:
        plan_text = plan_text_holder.get("text") or plan_file.read_text(
            encoding="utf-8"
        )
        set_stage("review-code")
        logger.info("[3/5] reviewing code (model: %s)", args.detail)
        review_file = run_review_code_stage(
            client=client,
            session_dir=session_dir,
            plan_text=plan_text,
            detail=args.detail,
            is_git=is_git,
            code_style=code_style,
        )
        logger.info("detail code review (%s): %s", args.detail, review_file)

    def stage_address_code() -> None:
        set_stage("address-code")
        logger.info("[4/5] addressing code reviews (model: %s)", args.address)
        address_code.run(
            ctx,
            address_code.AddressCodeOptions(
                address_model=args.address,
                is_git=is_git,
                code_style=code_style,
            ),
        )

    def stage_test() -> None:
        set_stage("test")
        if args.test_cmd:
            logger.info(
                "[5/5] running tests (cmd: %r, max-attempts: %s, model: %s)",
                args.test_cmd,
                args.test_max_attempts,
                args.test_fix_model,
            )
        test.run(
            ctx,
            test.TestOptions(
                test_cmd=args.test_cmd,
                max_attempts=args.test_max_attempts,
                test_fix_model=args.test_fix_model,
                is_git=is_git,
                cwd=pathlib.Path.cwd(),
                code_style=code_style,
            ),
        )

    stages = [
        ("plan", stage_plan),
        ("implement", stage_implement),
        ("review-code", stage_review_code),
        ("address-code", stage_address_code),
        ("test", stage_test),
    ]
    run_stages(stages, resume_from=args.resume_from)

    total_cost = getattr(client, "total_cost_usd", 0.0)
    if total_cost is not None and total_cost > 0:
        patch_state(total_cost_usd=total_cost)

    logger.info("done. session artifacts in: %s", session_dir)
    if total_cost is not None and total_cost > 0:
        logger.info("total cost: $%.4f", total_cost)
    return 0


def _parse_review_args(argv: list[str]) -> argparse.Namespace:
    usage = (
        "usage: gremlins.cli review [--dir <path>] [--plan <path>] [-b <detail-model>]"
    )
    parser = argparse.ArgumentParser(add_help=False, usage=usage)
    parser.add_argument("--dir", dest="dir", default=".")
    parser.add_argument("--plan", dest="plan", default=None)
    parser.add_argument("-b", dest="detail", default="sonnet")
    args = parser.parse_args(argv)
    if not MODEL_RE.match(args.detail):
        die(f"invalid model: {args.detail}")
    return args


def review_main(argv: list[str], *, client: ClaudeClient | None = None) -> int:
    configure_logging()
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
        code_style = load_prompts([BUNDLED_PROMPT_DIR / "code_style.md"])
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

    logger.info("reviewing code (model: %s)", args.detail)
    review_file = run_review_code_stage(
        client=client,
        session_dir=session_dir,
        plan_text=plan_text,
        detail=args.detail,
        is_git=is_git,
        code_style=code_style,
    )
    logger.info("detail code review (%s): %s", args.detail, review_file)
    return 0


def _parse_address_args(argv: list[str]) -> argparse.Namespace:
    usage = "usage: gremlins.cli address [--dir <path>] [-x <address-model>]"
    parser = argparse.ArgumentParser(add_help=False, usage=usage)
    parser.add_argument("--dir", dest="dir", default=".")
    parser.add_argument("-x", dest="address", default="sonnet")
    args = parser.parse_args(argv)
    if not MODEL_RE.match(args.address):
        die(f"invalid model: {args.address}")
    return args


def address_main(argv: list[str], *, client: ClaudeClient | None = None) -> int:
    configure_logging()
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
        code_style = load_prompts([BUNDLED_PROMPT_DIR / "code_style.md"])
    except (FileNotFoundError, ValueError) as exc:
        die(f"error loading prompt: {exc}")

    ctx = StageContext(client=client, session_dir=session_dir, gr_id=os.environ.get("GR_ID"))
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
