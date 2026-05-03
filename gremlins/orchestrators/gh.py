"""Orchestrator entry point for the gh pipeline."""

from __future__ import annotations

import argparse
import json
import logging
import os
import pathlib
import re
import shutil
import subprocess
import sys
from typing import NoReturn

from ..clients.claude import ClaudeClient, SubprocessClaudeClient
from ..gh_utils import extract_gh_url, get_repo, parse_issue_ref, view_issue
from ..git import DirtyOnly, HeadAdvanced
from ..logging_setup import configure_logging
from ..prompts import BUNDLED_PROMPT_DIR, load_prompts
from ..runner import install_signal_handlers, run_stages
from ..stages import (
    commit_pr,
    ghaddress,
    ghreview,
    implement,
    request_copilot,
    wait_ci,
    wait_copilot,
)
from ..stages.context import StageContext
from ..stages.implement import ImplStageResult
from ..state import patch_state, resolve_session_dir, resolve_state_file, set_stage

logger = logging.getLogger(__name__)

MODEL_RE = re.compile(r"^[A-Za-z0-9._-]+$")
REF_RE = re.compile(r"^[A-Za-z0-9._/#-]+$")

VALID_STAGES = [
    "plan",
    "implement",
    "commit-pr",
    "request-copilot",
    "ghreview",
    "wait-copilot",
    "ghaddress",
    "ci-gate",
]


def die(msg: str) -> NoReturn:
    sys.stderr.write(f"error: {msg}\n")
    sys.stderr.flush()
    sys.exit(1)


def _parse_gh_args(argv: list[str]) -> argparse.Namespace:
    usage = (
        "usage: gremlins.cli gh [-r <ref>] [--resume-from <stage>] "
        '[--plan <path|issue-ref>] [--spec <path>] [--model <model>] "<instructions>"'
    )
    parser = argparse.ArgumentParser(add_help=False, usage=usage)
    parser.add_argument("-r", dest="ref", default="")
    parser.add_argument("--resume-from", dest="resume_from", default=None)
    parser.add_argument("--plan", dest="plan_source", default=None)
    parser.add_argument("--spec", dest="spec_path", default=None)
    parser.add_argument("--model", dest="model", default=None)
    parser.add_argument("instructions", nargs="*")
    args = parser.parse_args(argv)

    if args.resume_from is not None and args.resume_from not in VALID_STAGES:
        die(
            f"invalid --resume-from: {args.resume_from} "
            f"(allowed: {' '.join(VALID_STAGES)})"
        )

    if args.plan_source:
        if args.instructions:
            die("--plan and positional instructions are mutually exclusive")
    else:
        if args.resume_from is None and not args.instructions:
            die(usage)

    if args.model and not MODEL_RE.match(args.model):
        die(f"invalid model: {args.model}")
    if args.ref and not REF_RE.match(args.ref):
        die(f"invalid -r ref: {args.ref} (allowed: A-Z a-z 0-9 . _ / # -)")

    return args


def _read_state_field(sf: pathlib.Path | None, field: str) -> str:
    if sf is None or not sf.exists():
        return ""
    try:
        data = json.loads(sf.read_text(encoding="utf-8"))
        return data.get(field) or ""
    except Exception:
        return ""


_parse_issue_ref = parse_issue_ref


def _fetch_issue_body(issue_num: str, repo: str) -> str:
    try:
        issue_data = view_issue(issue_num, repo)
    except RuntimeError as exc:
        die(f"could not fetch issue #{issue_num} body: {exc}")
    body = (issue_data.get("body") or "").strip()
    if not body:
        die(f"issue #{issue_num} has an empty body")
    return body


def _update_description_from_plan(
    plan_md: pathlib.Path, state_file: pathlib.Path | None
) -> None:
    if state_file is None or not state_file.exists():
        return
    try:
        data = json.loads(state_file.read_text(encoding="utf-8"))
        if data.get("description_explicit"):
            return
        lines = plan_md.read_text(encoding="utf-8").splitlines()[:50]
        h1 = ""
        for line in lines:
            m = re.match(r"^#+\s+(.+)", line)
            if m:
                h1 = m.group(1)[:60]
                break
        if h1:
            patch_state(description=h1)
    except Exception:
        pass


def _resolve_plan_source(
    *,
    plan_source: str,
    repo: str,
    plan_md: pathlib.Path,
    model: str | None,
    client: ClaudeClient,
    state_file: pathlib.Path | None,
) -> tuple[str, str, str]:
    """Resolve --plan <source> into (issue_url, issue_num, issue_body)."""
    if plan_md.exists() and plan_md.stat().st_size > 0:
        issue_url = _read_state_field(state_file, "issue_url")
        issue_num = _read_state_field(state_file, "issue_num")
        issue_body = plan_md.read_text(encoding="utf-8")
        label = f" (issue #{issue_num})" if issue_num else ""
        logger.info("[1/7] plan resumed from snapshot: %s%s", plan_md, label)
        return issue_url, issue_num, issue_body

    if pathlib.Path(plan_source).is_file():
        src = pathlib.Path(plan_source)
        if src.stat().st_size == 0:
            die(f"--plan: file is empty: {plan_source}")
        issue_body = src.read_text(encoding="utf-8")
        logger.info(
            "[1/7] plan supplied via --plan (file): %s — posting as GitHub issue",
            plan_source,
        )

        title_prompt = (
            "Produce a concise GitHub issue title (under 80 characters) "
            "summarizing the spec below. Output ONLY the title, nothing else."
            f"\n\n{issue_body}"
        )
        completed = client.run(
            title_prompt,
            label="plan-title",
            model=model,
        )
        parts = (completed.text_result or "").strip().splitlines()
        issue_title = parts[0][:80] if parts else ""
        if not issue_title:
            die("--plan: title agent returned empty output")

        r = subprocess.run(
            [
                "gh",
                "issue",
                "create",
                "--repo",
                repo,
                "--title",
                issue_title,
                "--body-file",
                plan_source,
            ],
            capture_output=True,
            text=True,
            check=False,
        )
        if r.returncode != 0:
            die(f"--plan: failed to create GitHub issue: {r.stderr.strip()}")
        create_out = r.stdout + r.stderr
        m = re.search(r"https://github\.com/[^ )]+/issues/[0-9]+", create_out)
        if not m:
            die(
                f"--plan: could not extract issue URL from gh output: {create_out.strip()}"
            )
        issue_url = m.group(0)
        issue_num = issue_url.split("/")[-1]

        patch_state(issue_url=issue_url, issue_num=issue_num)
        logger.info("issue: %s", issue_url)
        shutil.copyfile(src, plan_md)
    else:
        target_repo, issue_ref = _parse_issue_ref(plan_source, repo)
        if target_repo is None or issue_ref is None:
            die(
                f"--plan: not a readable file or recognized issue reference: {plan_source}"
            )

        try:
            issue_data = view_issue(issue_ref, target_repo)
        except RuntimeError as exc:
            die(f"--plan: {exc}")
        issue_body = issue_data.get("body") or ""
        if not issue_body:
            die(f"--plan: issue {plan_source} has an empty body")

        resolved_url = issue_data.get("url") or ""
        resolved_num = str(issue_data.get("number") or "")

        if target_repo == repo:
            issue_url = resolved_url
            issue_num = resolved_num
        else:
            issue_url = ""
            issue_num = ""

        plan_md.write_text(issue_body + "\n", encoding="utf-8")
        logger.info(
            "[1/7] plan supplied via --plan (issue %s#%s)", target_repo, issue_ref
        )

    patch_state(issue_url=issue_url, issue_num=issue_num)
    _update_description_from_plan(plan_md, state_file)
    return issue_url, issue_num, issue_body


def gh_main(argv: list[str], *, client: ClaudeClient | None = None) -> int:
    configure_logging()
    args = _parse_gh_args(argv)
    if os.environ.get("GREMLINS_TEST_NOOP_PIPELINE"):
        return 0

    if shutil.which("claude") is None:
        die("claude CLI not found")
    if shutil.which("gh") is None:
        die("gh CLI not found")

    if client is None:
        client = SubprocessClaudeClient()
    install_signal_handlers(client)

    repo = get_repo()
    session_dir = resolve_session_dir()
    state_file = resolve_state_file()
    plan_md = session_dir / "plan.md"
    spec_file = session_dir / "spec.md"

    logger.info("session: %s", session_dir)

    if args.spec_path and not spec_file.exists():
        spec_src = pathlib.Path(args.spec_path)
        if not spec_src.is_file():
            die(f"--spec: file not found: {args.spec_path}")
        if spec_src.stat().st_size == 0:
            die(f"--spec: file is empty: {args.spec_path}")
        shutil.copyfile(spec_src, spec_file)

    model = args.model
    if model is None:
        model = _read_state_field(state_file, "model") or "sonnet"
    if model:
        patch_state(model=model)

    instructions = " ".join(args.instructions) if args.instructions else ""

    issue_url: str = ""
    issue_num: str = ""
    issue_body: str = ""

    plan_stage_idx = VALID_STAGES.index("plan")
    resume_idx = VALID_STAGES.index(args.resume_from) if args.resume_from else 0

    if args.plan_source:
        issue_url, issue_num, issue_body = _resolve_plan_source(
            plan_source=args.plan_source,
            repo=repo,
            plan_md=plan_md,
            model=model,
            client=client,
            state_file=state_file,
        )
    elif resume_idx > plan_stage_idx:
        issue_url = _read_state_field(state_file, "issue_url")
        if not issue_url:
            die(
                f"--resume-from {args.resume_from}: no issue_url in state.json "
                "(rewind to plan?)"
            )
        issue_num = issue_url.split("/")[-1]
        logger.info("resumed issue: %s", issue_url)
        issue_body = _fetch_issue_body(issue_num, repo)

    try:
        code_style = load_prompts([BUNDLED_PROMPT_DIR / "code_style.md"])
    except (FileNotFoundError, ValueError) as exc:
        die(f"error loading prompt: {exc}")

    ctx = StageContext(
        client=client,
        session_dir=session_dir,
        gr_id=os.environ.get("GR_ID"),
    )

    # Inter-stage state
    impl_result: ImplStageResult | None = None
    pr_url: str = ""
    pr_num: str = ""

    def _ensure_pr_url() -> None:
        nonlocal pr_url, pr_num
        if pr_url:
            return
        saved = _read_state_field(state_file, "pr_url")
        if not saved:
            die(
                f"--resume-from {args.resume_from}: no pr_url in state.json "
                "(rewind to implement?)"
            )
        pr_url = saved
        pr_num = saved.split("/")[-1]
        logger.info("resumed PR: %s", saved)

    def stage_plan() -> None:
        nonlocal issue_url, issue_num, issue_body
        if args.plan_source:
            return
        set_stage("plan")
        logger.info("[1/7] running /ghplan")
        plan_prompt = f"/ghplan {args.ref + ' ' if args.ref else ''}{instructions}"
        completed = ctx.client.run(
            plan_prompt,
            label="plan",
            model=model,
            raw_path=session_dir / "ghplan-out.jsonl",
            capture_events=True,
        )
        events = completed.events or []
        issue_url = extract_gh_url(
            events,
            url_pattern=r"https://github\.com/[^ )]+/issues/[0-9]+",
            cmd_pattern=r"gh issue create",
            label="issue",
        )
        issue_num = issue_url.split("/")[-1]
        logger.info("issue: %s", issue_url)
        patch_state(issue_url=issue_url, issue_num=issue_num)
        issue_body = _fetch_issue_body(issue_num, repo)

    def stage_implement() -> None:
        nonlocal impl_result
        set_stage("implement")
        logger.info("[2a/7] implementing plan")
        spec_text = ""
        if spec_file.exists():
            try:
                spec_text = spec_file.read_text(encoding="utf-8")
            except (OSError, UnicodeDecodeError) as exc:
                logger.warning(
                    "could not read spec.md (%s); proceeding without north-star context",
                    exc,
                )
        impl_result = implement.run(
            ctx,
            implement.ImplementOptions(
                impl_model=model,
                plan_text=issue_body,
                code_style=code_style,
                is_git=True,
                kind="gh",
                issue_num=issue_num,
                spec_text=spec_text,
            ),
        )
        if impl_result is None:
            die("implement stage did not produce a result")
        patch_state(
            impl_handoff_branch=impl_result.handoff_branch,
            impl_base_ref=impl_result.pre_state.head,
        )

    def stage_commit_pr() -> None:
        nonlocal pr_url, pr_num
        set_stage("commit-pr")
        logger.info("[2b/7] committing + opening PR")

        if impl_result is not None:
            impl_outcome = impl_result.outcome
            impl_handoff_branch = impl_result.handoff_branch
            base_ref = impl_result.pre_state.head
        else:
            impl_handoff_branch = _read_state_field(state_file, "impl_handoff_branch")
            base_ref = _read_state_field(state_file, "impl_base_ref")
            if not base_ref:
                die(
                    "--resume-from commit-pr: no impl_base_ref in state.json "
                    "(rewind to implement?)"
                )
            if impl_handoff_branch:
                count_r = subprocess.run(
                    [
                        "git",
                        "rev-list",
                        "--count",
                        f"{base_ref}..{impl_handoff_branch}",
                    ],
                    capture_output=True,
                    text=True,
                    check=False,
                )
                if count_r.returncode != 0:
                    die(
                        f"--resume-from commit-pr: impl_handoff_branch '{impl_handoff_branch}' "
                        f"not found or base_ref invalid (rewind to implement?)\n"
                        f"{count_r.stderr.strip()}"
                    )
                commit_count = int(count_r.stdout.strip())
                impl_outcome = HeadAdvanced(commit_count=commit_count)
            else:
                impl_outcome = DirtyOnly()

        pr_url = commit_pr.run(
            ctx,
            commit_pr.CommitPrOptions(
                model=model,
                impl_outcome=impl_outcome,
                impl_handoff_branch=impl_handoff_branch,
                base_ref=base_ref,
                issue_url=issue_url,
                cwd=None,
            ),
        )
        pr_num = pr_url.split("/")[-1]
        logger.info("PR: %s", pr_url)
        patch_state(pr_url=pr_url)

    def stage_request_copilot() -> None:
        _ensure_pr_url()
        set_stage("request-copilot")
        logger.info("[3/7] requesting Copilot review")
        request_copilot.run(
            ctx,
            request_copilot.RequestCopilotOptions(repo=repo, pr_num=pr_num),
        )

    def stage_ghreview() -> None:
        _ensure_pr_url()
        set_stage("ghreview")
        logger.info("[4/7] running /ghreview")
        ghreview.run(
            ctx,
            ghreview.GhreviewOptions(
                model=model,
                pr_url=pr_url,
                code_style=code_style,
            ),
        )

    def stage_wait_copilot() -> None:
        _ensure_pr_url()
        set_stage("wait-copilot")
        logger.info("[5/7] waiting for Copilot review (20s interval, 10min timeout)")
        state = wait_copilot.run(
            ctx,
            wait_copilot.WaitCopilotOptions(repo=repo, pr_num=pr_num),
        )
        logger.info("Copilot review: %s", state)

    def stage_ghaddress() -> None:
        _ensure_pr_url()
        set_stage("ghaddress")
        logger.info("[6/7] running /ghaddress")
        ghaddress.run(
            ctx,
            ghaddress.GhaddressOptions(
                model=model,
                pr_url=pr_url,
                code_style=code_style,
            ),
        )

    def stage_wait_ci() -> None:
        _ensure_pr_url()
        set_stage("ci-gate")
        logger.info("[7/7] waiting for CI checks (up to 3 attempts, 20min each)")
        wait_ci.run(
            ctx,
            wait_ci.WaitCiOptions(
                model=model,
                pr_url=pr_url,
                code_style=code_style,
            ),
        )

    stages = [
        ("plan", stage_plan),
        ("implement", stage_implement),
        ("commit-pr", stage_commit_pr),
        ("request-copilot", stage_request_copilot),
        ("ghreview", stage_ghreview),
        ("wait-copilot", stage_wait_copilot),
        ("ghaddress", stage_ghaddress),
        ("ci-gate", stage_wait_ci),
    ]
    run_stages(stages, resume_from=args.resume_from)

    total_cost = getattr(client, "total_cost_usd", 0.0)
    if total_cost is not None and total_cost > 0:
        patch_state(total_cost_usd=total_cost)

    logger.info("done. PR: %s", pr_url or "(unknown)")
    if total_cost is not None and total_cost > 0:
        logger.info("total cost: $%.4f", total_cost)
    return 0
